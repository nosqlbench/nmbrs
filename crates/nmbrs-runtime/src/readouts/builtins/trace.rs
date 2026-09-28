// Copyright 2024-2026 Jonathan Shook
// SPDX-License-Identifier: Apache-2.0

//! `trace` — a minimal diagnostic readout that surfaces
//! every relevant [`ReadoutContext`] field as text for
//! whatever event triggered it.
//!
//! Two purposes:
//!
//! 1. **Test affordance.** Bind it to events in unit
//!    tests and assert that events fire with the expected
//!    context. Drops the burden of crafting bespoke
//!    assertion helpers per surface.
//! 2. **Reference implementation.** This is the smallest
//!    possible custom renderer — every method an author
//!    might want to read from `ReadoutContext` is read
//!    here, with one line of formatted output per field.
//!    A user building a fully custom render format can
//!    copy this file and edit it.
//!
//! `trace` does not branch on LOD or content mode — it
//! emits the same fields at every combination. This
//! intentionally violates SRD-63 §3.3's monotonicity
//! guidance for production readouts; `trace` is a
//! diagnostic, not a user-facing render, and uniformity
//! across LODs is what makes it useful for testing.

use std::fmt::Write as _;

use crate::lifecycle::SubjectKind;
use crate::readouts::buf::ReadoutBuf;
use crate::readouts::context::ReadoutContext;
use crate::readouts::readout::{ContentMode, Lod, Readout, ReadoutOptions};

pub struct Trace;

impl Readout for Trace {
    fn name(&self) -> &'static str {
        "trace"
    }
    fn accepts(&self) -> &'static [SubjectKind] {
        &[
            SubjectKind::Session,
            SubjectKind::Phase,
            SubjectKind::Iteration,
            SubjectKind::Scope,
        ]
    }

    fn render(
        &self,
        ctx: &dyn ReadoutContext,
        lod: Lod,
        mode: ContentMode,
        _opts: &ReadoutOptions,
        out: &mut dyn ReadoutBuf,
    ) -> usize {
        // Push 7: in Explanation mode, dump the field
        // *schema* (one line per field: `name: kind`)
        // rather than the live values. Same call shape
        // as Value — the user gets to see what every
        // field means without changing display position.
        if matches!(mode, ContentMode::Explanation) {
            return render_explanation(ctx, out);
        }
        let mut tmp = String::with_capacity(512);

        // Header: event slot + LOD + mode. Three pieces of
        // metadata that aren't part of `ReadoutContext`
        // itself — they describe the call shape.
        let _ = write!(
            &mut tmp,
            "event={slot} lod={lod:?} mode={mode:?}",
            slot = ctx.event().slot_name(),
        );

        // Context fields, one per line under the header.
        // Order kept stable across runs so test assertions
        // can pin against it.
        let _ = write!(&mut tmp, "\n  refresh_tick={t}", t = ctx.refresh_tick(),);
        let _ = write!(&mut tmp, "\n  phase_name={n:?}", n = ctx.subject_name(),);
        let _ = write!(&mut tmp, "\n  activity_name={n:?}", n = ctx.activity_name(),);
        match ctx.subject_seq() {
            Some((i, t)) => {
                let _ = write!(&mut tmp, "\n  phase_seq=({i}/{t})");
            }
            None => {
                let _ = write!(&mut tmp, "\n  phase_seq=None");
            }
        }
        let _ = write!(&mut tmp, "\n  phase_labels={l:?}", l = ctx.subject_labels(),);
        let _ = write!(
            &mut tmp,
            "\n  cycles={c}/{t}",
            c = ctx.cycles_completed(),
            t = ctx.cycles_total(),
        );
        let _ = write!(
            &mut tmp,
            "\n  ops_started={s} ops_ok={ok} errors={e} retries={r}",
            s = ctx.ops_started(),
            ok = ctx.ops_ok(),
            e = ctx.errors(),
            r = ctx.retries(),
        );
        let _ = write!(&mut tmp, "\n  concurrency={c}", c = ctx.concurrency(),);
        let _ = write!(&mut tmp, "\n  consumed={c}", c = ctx.consumed(),);
        let _ = write!(&mut tmp, "\n  elapsed_secs={e:.3}", e = ctx.elapsed_secs(),);
        match ctx.eta_secs() {
            Some(s) => {
                let _ = write!(&mut tmp, "\n  eta_secs={s:.3}");
            }
            None => {
                let _ = write!(&mut tmp, "\n  eta_secs=None");
            }
        }
        let chips = ctx.status_metric_chips();
        if !chips.is_empty() {
            let _ = write!(&mut tmp, "\n  status_metric_chips={chips:?}");
        }
        let adapter = ctx.adapter_counters_text();
        if !adapter.is_empty() {
            let _ = write!(&mut tmp, "\n  adapter_counters_text={adapter:?}");
        }
        let batch = ctx.batch_info_text();
        if !batch.is_empty() {
            let _ = write!(&mut tmp, "\n  batch_info_text={batch:?}");
        }
        let _ = write!(
            &mut tmp,
            "\n  depth_indent_len={d}",
            d = ctx.depth_indent().len(),
        );
        let _ = write!(&mut tmp, "\n  use_color={c}", c = ctx.use_color(),);

        let len = tmp.len();
        let _ = out.write_str(&tmp);
        len
    }
}

/// Schema dump for Explanation mode. Same field list as
/// the value render, but each row reads
/// `name: <semantic type>` instead of `name=<value>`.
fn render_explanation(ctx: &dyn ReadoutContext, out: &mut dyn ReadoutBuf) -> usize {
    let s = "event=on_<event-slot> lod=<density> mode=<value|explanation>\n  \
refresh_tick: monotonic counter advanced once per refresh fire\n  \
phase_name: bare phase identifier (no coord suffix)\n  \
activity_name: full activity name with leaf coord\n  \
phase_seq: pre-map (idx, total) numbering\n  \
phase_labels: root-first scope coordinate path\n  \
cycles: completed/total\n  \
ops_started ops_ok errors retries: cumulative counters\n  \
concurrency: effective fiber count\n  \
consumed: items pulled from the source factory\n  \
elapsed_secs: wallclock since phase start\n  \
eta_secs: estimated remaining time, when computable\n  \
status_metric_chips: workload-emphasised metric tail\n  \
adapter_counters_text: per-dispenser counter chips\n  \
batch_info_text: rows-per-batch summary\n  \
depth_indent_len: scope-tree indent width\n  \
use_color: surface accepts ANSI styling"
        .to_string();
    let _ = out.write_str(&s);
    let _ = ctx; // schema doesn't depend on context
    s.len()
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::lifecycle::EventType;
    use crate::readouts::buf::StringBuf;

    #[derive(Default)]
    struct TestCtx {
        event: Option<EventType>,
        refresh_tick: u64,
        phase_name: String,
        activity_name: Option<String>,
        phase_seq: Option<(usize, usize)>,
        phase_labels: String,
        cycles_completed: u64,
        cycles_total: u64,
        ops_started: u64,
        ops_ok: u64,
        errors: u64,
        retries: u64,
        concurrency: usize,
        consumed: u64,
        elapsed_secs: f64,
        eta_secs: Option<f64>,
        chips: String,
        adapter: String,
        batch: String,
        depth_indent: String,
        use_color: bool,
    }

    impl ReadoutContext for TestCtx {
        fn subject_name(&self) -> &str {
            &self.phase_name
        }
        fn activity_name(&self) -> &str {
            self.activity_name.as_deref().unwrap_or(&self.phase_name)
        }
        fn subject_seq(&self) -> Option<(usize, usize)> {
            self.phase_seq
        }
        fn subject_labels(&self) -> &str {
            &self.phase_labels
        }
        fn cycles_completed(&self) -> u64 {
            self.cycles_completed
        }
        fn cycles_total(&self) -> u64 {
            self.cycles_total
        }
        fn ops_started(&self) -> u64 {
            self.ops_started
        }
        fn ops_ok(&self) -> u64 {
            self.ops_ok
        }
        fn errors(&self) -> u64 {
            self.errors
        }
        fn retries(&self) -> u64 {
            self.retries
        }
        fn concurrency(&self) -> usize {
            self.concurrency
        }
        fn elapsed_secs(&self) -> f64 {
            self.elapsed_secs
        }
        fn eta_secs(&self) -> Option<f64> {
            self.eta_secs
        }
        fn consumed(&self) -> u64 {
            self.consumed
        }
        fn status_metric_chips(&self) -> String {
            self.chips.clone()
        }
        fn adapter_counters_text(&self) -> String {
            self.adapter.clone()
        }
        fn batch_info_text(&self) -> String {
            self.batch.clone()
        }
        fn depth_indent(&self) -> &str {
            &self.depth_indent
        }
        fn use_color(&self) -> bool {
            self.use_color
        }
        fn event(&self) -> EventType {
            self.event.unwrap_or(EventType::PhaseEnd)
        }
        fn refresh_tick(&self) -> u64 {
            self.refresh_tick
        }
    }

    fn render(ctx: &TestCtx) -> String {
        let mut s = String::new();
        let mut buf = StringBuf::new(&mut s);
        Trace.render(
            ctx,
            Lod::Labeled,
            ContentMode::Value,
            &ReadoutOptions::new(),
            &mut buf,
        );
        s
    }

    #[test]
    fn covers_every_event_class() {
        // Every Event variant must round-trip through the
        // header. If a future Event variant gets added and
        // the reverse-mapping forgets it, this test fails.
        for ev in [
            EventType::SessionStart,
            EventType::SessionEnd,
            EventType::PhaseStart,
            EventType::PhaseEnd,
            EventType::EachStart,
            EventType::EachEnd,
            EventType::ScopeStart,
            EventType::ScopeEnd,
            EventType::Update,
        ] {
            let ctx = TestCtx {
                event: Some(ev),
                phase_name: "p".into(),
                ..Default::default()
            };
            let out = render(&ctx);
            assert!(
                out.starts_with(&format!("event={}", ev.slot_name())),
                "event {ev:?} not surfaced in trace header: {out}"
            );
        }
    }

    #[test]
    fn dumps_all_listed_fields() {
        let ctx = TestCtx {
            event: Some(EventType::PhaseEnd),
            refresh_tick: 7,
            phase_name: "ann_query".into(),
            activity_name: Some("ann_query (k=10)".into()),
            phase_seq: Some((3, 9)),
            phase_labels: "(profile=alpha)".into(),
            cycles_completed: 100,
            cycles_total: 100,
            ops_started: 100,
            ops_ok: 99,
            errors: 1,
            retries: 0,
            concurrency: 4,
            consumed: 100,
            elapsed_secs: 1.234,
            eta_secs: Some(0.5),
            chips: " recall_at_10:79.62%".into(),
            adapter: " rows/s=12.5K".into(),
            batch: " r/b=12.5".into(),
            depth_indent: "    ".into(), // 4 spaces
            use_color: true,
        };
        let out = render(&ctx);
        // Spot-check every field surfaces.
        for needle in [
            "event=on_phase_end",
            "refresh_tick=7",
            "phase_name=\"ann_query\"",
            "activity_name=\"ann_query (k=10)\"",
            "phase_seq=(3/9)",
            "phase_labels=\"(profile=alpha)\"",
            "cycles=100/100",
            "ops_started=100 ops_ok=99 errors=1 retries=0",
            "concurrency=4",
            "consumed=100",
            "elapsed_secs=1.234",
            "eta_secs=0.500",
            "status_metric_chips=\" recall_at_10:79.62%\"",
            "adapter_counters_text=\" rows/s=12.5K\"",
            "batch_info_text=\" r/b=12.5\"",
            "depth_indent_len=4",
            "use_color=true",
        ] {
            assert!(
                out.contains(needle),
                "trace missing {needle:?} — actual: {out}"
            );
        }
    }

    #[test]
    fn omits_empty_string_fields() {
        let ctx = TestCtx {
            phase_name: "x".into(),
            ..Default::default()
        };
        let out = render(&ctx);
        // Empty chips / adapter / batch suppressed.
        assert!(
            !out.contains("status_metric_chips="),
            "empty chips should not surface: {out}"
        );
        assert!(
            !out.contains("adapter_counters_text="),
            "empty adapter text should not surface: {out}"
        );
        assert!(
            !out.contains("batch_info_text="),
            "empty batch text should not surface: {out}"
        );
    }
}

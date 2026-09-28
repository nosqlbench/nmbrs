// Copyright 2024-2026 Jonathan Shook
// SPDX-License-Identifier: Apache-2.0

//! SRD-92 display-contract E2E — the key-metric gutter cell (R4) and
//! the `completed_phases=` scrollback retention knob (R5), driven
//! through a `shadow_terminal` PTY against the real `nmbrs run` so the
//! asserted cells are exactly what an operator sees.
//!
//! The workload is fully generic: a `testkit` op returns a fixed
//! 3-row keyed body scored against a literal ground-truth binding by
//! the standard `evaluations.relevancy` pipeline, producing a
//! DETERMINISTIC recall of 2/3 (keys {0,1,5} vs truth {0,1,2} at
//! k=3) — no live backend, no dataset, no timing dependence in the
//! measured value.
//!
//! Operating envelope: the child renders a live display, so total CPU
//! oversubscription (e.g. a spin-storm saturating every core,
//! including the one the child's timing/cadence thread pins to)
//! starves the render cadence and times these out — failing SAFE with
//! a screen dump and a killed child (see `pty_support`), never
//! wedging. Normal conditions, including full-workspace sweeps,
//! complete in well under a second.

use std::ffi::OsString;
use std::path::{Path, PathBuf};
use std::time::Duration;

use shadow_terminal::shadow_terminal::Config;
use shadow_terminal::steppable_terminal::SteppableTerminal;

mod pty_support;

fn nmbrs_binary() -> PathBuf {
    PathBuf::from(env!("CARGO_BIN_EXE_nmbrs"))
}

struct TempDir {
    path: PathBuf,
}
impl TempDir {
    fn new() -> Self {
        let pid = std::process::id();
        let nanos = std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .unwrap()
            .as_nanos();
        let path =
            std::env::temp_dir().join(format!("nmbrs-srd92-{pid}-{nanos}-{}", scratch::seq()));
        std::fs::create_dir_all(&path).expect("create tempdir");
        Self { path }
    }
    fn path(&self) -> &Path {
        &self.path
    }
}
impl Drop for TempDir {
    fn drop(&mut self) {
        let _ = std::fs::remove_dir_all(&self.path);
    }
}

/// One phase whose op is scored by `evaluations.relevancy` against a
/// literal ground truth: recall = |{0,1,2} ∩ {0,1,5}| / 3 = 66.67%,
/// same value every cycle. `pace_ms` stretches the phase so the live
/// footer (metric cell) is observable; the retention tests run it
/// fast instead.
/// `with_epilogue` appends a ~4 s paced trailer phase: the retention
/// tests assert on `measure`'s completed block in SCROLLBACK, and a
/// child that exits milliseconds after printing it loses the tail to
/// the pty drain race (the emulator can miss bytes written just
/// before exit). The trailer keeps the child alive while the
/// assertions read a fully-drained block.
fn recall_workload(dir: &TempDir, cycles: u32, pace_ms: u32, with_epilogue: bool) -> PathBuf {
    let epilogue_phase = if with_epilogue { ", epilogue" } else { "" };
    let epilogue_block = if with_epilogue {
        "\n  epilogue:\n    adapter: testkit\n    cycles: 40\n    concurrency: 1\n    ops:\n      linger:\n        stmt: \"tick\"\n        result-latency: \"100ms\"\n"
    } else {
        ""
    };
    let yaml_path = dir.path().join("recall_cell.yaml");
    std::fs::write(
        &yaml_path,
        format!(
            r#"scenarios:
  default: [measure{epilogue_phase}]

phases:
  measure:
    adapter: testkit
    cycles: {cycles}
    concurrency: 1
    status_metrics: [recall]
    bindings: |
      input cycle: u64
      ground_truth := "0,1,2"
    ops:
      probe:
        stmt: "ping"
        result-latency: "{pace_ms}ms"
        readout: visible
        result-body:
          - key: "0"
          - key: "1"
          - key: "5"
        evaluations:
          relevancy:
            actual: key
            expected: ground_truth
            k: 3
            r: 3
            functions: [recall]{epilogue_block}
"#
        ),
    )
    .expect("write workload yaml");
    yaml_path
}

/// The `measure` block's slice of the screen: from its terminal
/// (✓/⊘/✗) header row up to the first subsequent `epilogue` row (the
/// trailer phase's footer). Assertions scoped here can't be satisfied
/// or violated by the trailer's own live rows.
fn measure_block(screen: &str) -> Option<String> {
    let lines: Vec<&str> = screen.lines().collect();
    let start = lines.iter().position(|l| l.contains("[measure]"))?;
    let end = lines[start..]
        .iter()
        .position(|l| l.contains("epilogue"))
        .map(|off| start + off)
        .unwrap_or(lines.len());
    Some(lines[start..end].join("\n"))
}

fn build_config(workload: &Path, session: &Path, extra: &[&str]) -> Config {
    let mut command: Vec<OsString> = Vec::new();
    command.push(nmbrs_binary().into());
    command.push("run".into());
    command.push(format!("workload={}", workload.display()).into());
    command.push("--session-path".into());
    command.push(session.into());
    command.push("tui=terminal".into());
    for a in extra {
        command.push((*a).into());
    }
    Config {
        width: 200,
        height: 60,
        command,
        scrollback_size: 2000,
        scrollback_step: 10,
    }
}

/// Poll the rendered screen until `pred` holds, or kill the child
/// and panic with a dump (shared drive loop — see `pty_support`).
async fn assert_screen(
    stepper: &mut SteppableTerminal,
    what: &str,
    timeout: Duration,
    pred: impl Fn(&str) -> bool,
) {
    pty_support::wait_until(stepper, pred, timeout, what).await;
}

/// Wait until `measure`'s completed block satisfies `block_pred`,
/// give the drain a settle window, and return the block's slice.
/// The child is still alive (epilogue trailer running), so the
/// slice is fully drained — no dependence on output written just
/// before process exit.
async fn settled_measure_block(
    stepper: &mut SteppableTerminal,
    what: &str,
    block_pred: impl Fn(&str) -> bool,
) -> String {
    assert_screen(stepper, what, Duration::from_secs(45), |s| {
        measure_block(s).is_some_and(|b| block_pred(&b))
    })
    .await;
    // Settle: further drain passes so any straggler detail rows land
    // before negative assertions read the slice.
    for _ in 0..10 {
        pty_support::drain(stepper).await;
        tokio::time::sleep(Duration::from_millis(50)).await;
    }
    let screen = stepper.screen_as_string().unwrap_or_default();
    measure_block(&screen).expect("measure block present after wait")
}

/// SRD-92 R4 — a line whose LEFT-OF-DIVIDER cell carries the metric
/// macro: the metric's name plus a sparkline trend. The current
/// numeric must NOT be in the cell (single placement — it lives in
/// the row body's chips right of the divider), and because this
/// workload's recall is CONSTANT, an established trend (≥6 samples)
/// must render FLAT — a varying spark here is the degenerate-range
/// amplification bug (float accumulation noise stretched to a
/// full-height false trend).
fn has_recall_metric_cell(screen: &str) -> bool {
    const SPARK: &[char] = &['▁', '▂', '▃', '▄', '▅', '▆', '▇', '█'];
    screen.lines().any(|line| {
        let Some(bar) = line.find('│') else {
            return false;
        };
        let head = &line[..bar];
        if !head.contains("recall") || head.contains('%') {
            return false;
        }
        let glyphs: Vec<char> = head.chars().filter(|c| SPARK.contains(c)).collect();
        glyphs.len() >= 6 && glyphs.iter().all(|&c| c == glyphs[0])
    })
}

/// SRD-92 R4 — the key-metric gutter cell renders LIVE for a phase
/// whose `status_metrics:` selects a relevancy aggregate: a bright
/// trend sparkline labeled `recall` in the margin beside the
/// key-metrics row, while the numeric chip (`recall:66.67%`) stays in
/// the row body.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn recall_metric_cell_renders_live_trend() {
    let dir = TempDir::new();
    // 60 × 150 ms ≈ 9 s of live footer to observe.
    let yaml = recall_workload(&dir, 60, 150, false);
    let session = dir.path().join("session");
    let config = build_config(&yaml, &session, &[]);

    let mut stepper = SteppableTerminal::start(config)
        .await
        .expect("start steppable terminal");

    // The numeric chip lands in the row BODY (right of the divider).
    assert_screen(
        &mut stepper,
        "recall chip in row body",
        Duration::from_secs(45),
        |s| {
            s.lines().any(|line| {
                line.find('│')
                    .is_some_and(|bar| line[bar..].contains("recall:66.67%"))
            })
        },
    )
    .await;

    // The metric macro cell: name + spark trend, numeric-free, in the
    // margin (left of the divider).
    assert_screen(
        &mut stepper,
        "metric macro cell (`recall` + spark, no numeric) left of the divider",
        Duration::from_secs(45),
        has_recall_metric_cell,
    )
    .await;

    let _ = stepper.kill();
}

/// SRD-92 R5 default (`completed_phases=full`) — the completed
/// block is preserved in contract shape: ✓ header, counters detail
/// row (with the key-metric chip), the relevancy summary detail
/// line, and the op leaf.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn completed_full_keeps_detail_rows_and_leaves() {
    let dir = TempDir::new();
    let yaml = recall_workload(&dir, 3, 10, true);
    let session = dir.path().join("session");
    let config = build_config(&yaml, &session, &[]);

    let mut stepper = SteppableTerminal::start(config)
        .await
        .expect("start steppable terminal");
    // The op leaf is the LAST row of the retained block to land, so
    // waiting for it inside the slice means the whole block is in.
    let block = settled_measure_block(
        &mut stepper,
        "measure's full completed block (through the op leaf)",
        |b| b.contains("\u{2713} probe"),
    )
    .await;
    let _ = stepper.kill();

    assert!(
        block.contains("/s ok:"),
        "counters detail row must be retained under `full`:\n{block}"
    );
    assert!(
        block.contains("recall:66.67%"),
        "key-metric chip must be retained on the counters row under `full`:\n{block}"
    );
    assert!(
        block.contains("mean="),
        "relevancy summary detail line must be retained under `full`:\n{block}"
    );
}

/// SRD-92 R5 `completed_phases=headers` — scrollback keeps ONLY the
/// ✓ header row: the counters detail row, the relevancy summary
/// (PhaseDetail) line, and the op leaves are all dropped from the
/// display surface (session.log keeps them unconditionally).
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn completed_headers_drops_detail_rows_and_leaves() {
    let dir = TempDir::new();
    let yaml = recall_workload(&dir, 3, 10, true);
    let session = dir.path().join("session");
    let config = build_config(&yaml, &session, &["completed_phases=headers"]);

    let mut stepper = SteppableTerminal::start(config)
        .await
        .expect("start steppable terminal");
    // The header row itself is the wait target; the settle window in
    // the helper gives any (wrongly) emitted detail rows time to land
    // before the absence assertions read the slice.
    let block = settled_measure_block(&mut stepper, "measure's ✓ header row", |_| true).await;
    let _ = stepper.kill();

    assert!(
        !block.contains("/s ok:"),
        "counters detail row must be dropped under `headers`:\n{block}"
    );
    assert!(
        !block.contains("mean="),
        "relevancy summary detail line must be dropped under `headers`:\n{block}"
    );
    assert!(
        !block.contains("✓ probe"),
        "op leaf must be dropped under `headers`:\n{block}"
    );

    // The dropped rows still land in session.log (display-surface
    // retention only — the durable record is not thinned).
    let log = std::fs::read_to_string(session.join("session.log")).unwrap_or_default();
    assert!(
        log.contains("mean="),
        "session.log must keep the relevancy summary regardless of \
         completed_phases:\n{log}"
    );
}

mod scratch;

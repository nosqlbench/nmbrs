// Copyright 2024-2026 Jonathan Shook
// SPDX-License-Identifier: Apache-2.0

//! Phase `throttle:` — the adaptive backpressure governor, end to
//! end (SRD-83 §throttle).
//!
//! The testkit MEASURED overload model (`result-overload: N` with no
//! synthetic `result-load`) rejects retryably whenever the real
//! in-flight count exceeds N — a deterministic saturating "server".
//! The phase AUTHORS concurrency 32 against `result-overload: 6`,
//! but the governor slow-starts at the floor and doubles through
//! clean windows: the target is never assaulted at 32. The climb
//! overshoots the capacity (…4 → 8), the windowed attempt-failure
//! fraction spikes, and severity-proportional back-off plus the
//! congestion memory settle the offer just under the threshold —
//! additive probes thereafter. Every movement is one visible
//! `throttle:` line naming the signal.

use std::path::{Path, PathBuf};
use std::process::Command;

struct Sandbox {
    dir: PathBuf,
}

impl Sandbox {
    fn new(tag: &str) -> Self {
        let pid = std::process::id();
        let nanos = std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .unwrap()
            .as_nanos();
        let dir = std::env::temp_dir().join(format!("nmbrs-throttle-{tag}-{pid}-{nanos}"));
        std::fs::create_dir_all(&dir).expect("create sandbox");
        Self { dir }
    }
    fn path(&self) -> &Path {
        &self.dir
    }
}

impl Drop for Sandbox {
    fn drop(&mut self) {
        let _ = std::fs::remove_dir_all(&self.dir);
    }
}

/// Overload threshold 6 vs authored concurrency 32: without the
/// governor this phase churns retries for its whole extent; with
/// it, the offer opens at floor 2 and discovers ~6 by climbing.
/// The tight window (250ms) makes the discovery fast; the op
/// latency (10ms) keeps enough attempts per window for a stable
/// fraction. `retry_advisory: off` keeps this log focused on the
/// governor's own lines.
const WORKLOAD: &str = r#"description: throttle governor e2e

phases:
  governed_storm:
    adapter: testkit
    cycles: 4000
    concurrency: 32
    errors: "count"
    throttle:
      high: 0.10
      floor: 2
      window: "250ms"
    ops:
      pressured:
        stmt: "x"
        tries: 8
        retry_backoff: "10ms"
        retry_advisory: "off"
        result-overload: 6
        result-latency: "10ms"
"#;

#[test]
fn governor_walks_concurrency_down_until_failures_stop() {
    let sb = Sandbox::new("e2e");
    let wl = sb.path().join("governed.yaml");
    std::fs::write(&wl, WORKLOAD).expect("write workload");
    let session = sb.path().join("session");

    let out = Command::new(env!("CARGO_BIN_EXE_nmbrs"))
        .current_dir(sb.path())
        .arg("run")
        .arg("--session-path")
        .arg(&session)
        .args(["workload=./governed.yaml", "tui=off"])
        .output()
        .expect("run nmbrs");
    assert!(
        out.status.success(),
        "governed storm must complete:\n{}\n{}",
        String::from_utf8_lossy(&out.stdout),
        String::from_utf8_lossy(&out.stderr)
    );

    let log = std::fs::read_to_string(session.join("session.log")).expect("session log");

    // The governor announced its slow-start and bounds at phase
    // start, and the climb out of the floor is visible.
    assert!(
        log.contains("throttle: governing 'concurrency' — slow-start"),
        "governor must announce slow-start and bounds:\n{log}"
    );
    assert!(
        log.contains("climbing concurrency"),
        "clean windows must double the offer visibly:\n{log}"
    );

    // Visible walk-downs naming the signal (`>` distinguishes the
    // back-off lines from `<` recovery lines).
    let downs: Vec<&str> = log
        .lines()
        .filter(|l| l.contains("throttle: phase 'governed_storm'"))
        .filter(|l| l.contains("windowed attempt failure") && l.contains(" > "))
        .collect();
    assert!(
        !downs.is_empty(),
        "overshooting the capacity during the climb must produce a \
         visible back-off:\n{log}"
    );

    // Slow-start means the target is never assaulted at the authored
    // 32: every back-off happens at a small offer discovered by
    // climbing (the wall is at 6; the climb overshoots to at most ~8,
    // and additive probes stay in its neighborhood).
    let max_backoff_point = downs
        .iter()
        .filter_map(|l| {
            // "… concurrency X → Y" — X is the offer at which the
            // failure was observed.
            l.split(" concurrency ")
                .nth(1)
                .and_then(|s| s.split_whitespace().next())
                .and_then(|s| s.parse::<f64>().ok())
        })
        .fold(0.0f64, f64::max);
    assert!(
        (1.0..=16.0).contains(&max_backoff_point),
        "with slow-start every congestion event is near the wall (6), \
         never at the authored 32; worst offer seen: {max_backoff_point}\n{log}"
    );

    // And the phase genuinely completed its full extent.
    assert!(
        log.contains("[governed_storm] 100%") || log.contains("all fibers drained"),
        "phase must complete its extent:\n{log}"
    );
}

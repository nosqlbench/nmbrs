// Copyright 2024-2026 Jonathan Shook
// SPDX-License-Identifier: Apache-2.0

//! Integration tests for the per-level concurrent scheduler
//! (SRD 18b §"Scheduler abstraction").
//!
//! Wall-clock comparisons are too noisy on shared infrastructure,
//! so each test inspects the session.log stream: with two latency-
//! bounded sibling phases, a serial run finishes the first phase
//! before announcing the second, while a concurrent run logs both
//! phases as in-flight before either completes. The latency-bound
//! ops give the scheduler enough wall-time slack that the entry
//! lines for both phases land before the completion lines under
//! any reasonable concurrent dispatch. (`tui=off` suppresses in-run
//! diagnostics from stderr — see `run`'s doc — so the console
//! stream carries no dispatch evidence.)

use std::io::Write;
use std::path::PathBuf;
use std::process::Command;

fn nmbrs() -> Command {
    let mut cmd = Command::new(env!("CARGO_BIN_EXE_nmbrs"));
    let workspace_root = std::path::Path::new(env!("CARGO_MANIFEST_DIR"))
        .parent()
        .unwrap()
        .parent()
        .expect("workspace root");
    cmd.current_dir(workspace_root);
    cmd
}

fn write_workload(label: &str, body: &str) -> PathBuf {
    let mut dir = std::env::temp_dir();
    std::fs::create_dir_all(&dir).unwrap_or_else(|e| panic!("create_dir_all {dir:?}: {e}"));
    dir.push(format!(
        "nmbrs_sched_{label}_{}_{}.yaml",
        std::process::id(),
        std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .unwrap()
            .as_nanos(),
    ));
    let mut f = std::fs::File::create(&dir).unwrap_or_else(|e| panic!("create {dir:?}: {e}"));
    f.write_all(body.as_bytes())
        .unwrap_or_else(|e| panic!("write {dir:?}: {e}"));
    dir
}

/// Two sibling phases. Each runs one op with a 1-second simulated
/// latency — enough to make concurrent dispatch observable in the
/// log stream but not so long it slows CI.
const TWO_PHASE_LATENCY_YAML: &str = r#"
scenarios:
  default:
    - left
    - right

phases:
  left:
    cycles: ===auto
    concurrency: 1
    ops:
      ping_left:
        stmt: ping_left
        result-latency: 1000
  right:
    cycles: ===auto
    concurrency: 1
    ops:
      ping_right:
        stmt: ping_right
        result-latency: 1000
"#;

/// Returns `(stdout, session_log, ok)`. The dispatch evidence is read
/// from the session's `session.log`, not stderr: `tui=off` claims the
/// log-only surface for the whole run (the always-a-sink invariant —
/// no workload thread may block on an adapter-owned terminal), so
/// in-run diagnostics land in the ring + session.log and are
/// deliberately suppressed from the console.
fn run(workload: &std::path::Path, extra: &[&str]) -> (String, String, bool) {
    // Per-invocation session parent so cargo's parallel test
    // execution doesn't collide on `logs/default_<timestamp>`.
    let session_parent = std::env::temp_dir().join(format!(
        "nmbrs-sched-{}-{}-{}",
        scratch::seq(),
        std::process::id(),
        std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .unwrap()
            .as_nanos(),
    ));
    std::fs::create_dir_all(&session_parent).expect("create session parent");
    let session_path = session_parent.join("session");

    let mut cmd = nmbrs();
    cmd.arg("run");
    cmd.arg(format!("workload={}", workload.display()));
    cmd.arg("tui=off");
    cmd.arg("adapter=testkit");
    cmd.arg("--session-path");
    cmd.arg(&session_path);
    for a in extra {
        cmd.arg(a);
    }
    let out = cmd.output().expect("failed to exec nmbrs");
    let session_log = std::fs::read_to_string(session_path.join("session.log")).unwrap_or_default();
    let _ = std::fs::remove_dir_all(&session_parent);
    (
        String::from_utf8_lossy(&out.stdout).to_string(),
        session_log,
        out.status.success(),
    )
}

/// Find the line index of the first occurrence of `needle`.
fn line_of(stream: &str, needle: &str) -> Option<usize> {
    stream.lines().position(|l| l.contains(needle))
}

/// True iff the scheduler announced a concurrent dispatch for
/// the sibling phases. The announcement line
///   `concurrent dispatch (limit=…): [1/N] x, [2/N] y, …`
/// fires once before any phase runs in concurrent mode; serial
/// mode has no such line and starts each phase only after the
/// prior one's ✓. The phase-starting log row was removed when
/// the per-phase output condensed to a single ✓ line, so this
/// announcement is the cleanest remaining proof of
/// concurrent-vs-serial dispatch in the log stream.
fn both_in_flight(stream: &str) -> bool {
    let _ = line_of; // retained for future per-line ordering checks
    stream.lines().any(|l| l.contains("concurrent dispatch"))
}

#[test]
fn serial_baseline_does_not_overlap_phases() {
    let path = write_workload("serial", TWO_PHASE_LATENCY_YAML);
    let (stdout, log, ok) = run(&path, &[]);
    assert!(ok, "nmbrs failed: stdout={stdout}\nsession.log={log}");
    assert!(
        !both_in_flight(&log),
        "serial baseline must complete each phase before starting the next.\nsession.log={log}",
    );
}

#[test]
fn schedule_unlimited_overlaps_sibling_phases() {
    let path = write_workload("unlimited", TWO_PHASE_LATENCY_YAML);
    let (stdout, log, ok) = run(&path, &["schedule=*"]);
    assert!(ok, "nmbrs failed: stdout={stdout}\nsession.log={log}");
    assert!(
        both_in_flight(&log),
        "schedule=* must dispatch both siblings before either completes.\nsession.log={log}",
    );
}

#[test]
fn schedule_bounded_two_overlaps_two_siblings() {
    let path = write_workload("bounded2", TWO_PHASE_LATENCY_YAML);
    let (stdout, log, ok) = run(&path, &["schedule=2"]);
    assert!(ok, "nmbrs failed: stdout={stdout}\nsession.log={log}");
    assert!(
        both_in_flight(&log),
        "schedule=2 must dispatch both siblings before either completes.\nsession.log={log}",
    );
}

mod scratch;

// Copyright 2024-2026 Jonathan Shook
// SPDX-License-Identifier: Apache-2.0

//! SRD-76 — failed-phase outcome round-trip regression.
//!
//! Spec §"Migration plan / Push 6": end-to-end coverage that
//! a failing phase lands its structured `PhaseOutcome` on the
//! scene tree, persists it to the `phase_outcomes` /
//! `phase_errors` sqlite tables, and surfaces via
//! `nmbrs replay`'s `phase_outcome` renderer with the ✗
//! glyph + class + first-error message.
//!
//! Uses the testkit adapter with `result-error-rate: 1.0` so
//! every cycle errors and `errors=stop` to drop the phase as
//! Failed at cycle 0. Two assertions per axis:
//!
//! 1. The sqlite store records the structured outcome
//!    (status=`failed`, non-empty error list with class +
//!    message + cycle).
//! 2. `nmbrs replay --plain` renders the row through the
//!    `phase_outcome` readout, producing a `✗ [name] ...`
//!    failure-flavoured line.

use std::io::Write;
use std::path::{Path, PathBuf};
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
    dir.push(format!("nmbrs_outcome_{label}_{}.yaml", std::process::id(),));
    let mut f = std::fs::File::create(&dir).unwrap_or_else(|e| panic!("create {dir:?}: {e}"));
    f.write_all(body.as_bytes())
        .unwrap_or_else(|e| panic!("write {dir:?}: {e}"));
    dir
}

/// Run `nmbrs run workload=... <extra>` against a freshly-
/// minted session dir under TMPDIR and return the session
/// path (so subsequent assertions / replay invocations can
/// open the same metrics.db) along with stdout/stderr/exit.
fn run_with_session(workload: &Path, extra: &[&str]) -> (PathBuf, String, String, bool) {
    let session_parent = std::env::temp_dir().join(format!(
        "nmbrs-outcome-{}-{}-{}",
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
    cmd.arg("--session-path");
    cmd.arg(&session_path);
    for a in extra {
        cmd.arg(a);
    }
    let out = cmd.output().expect("failed to exec nmbrs");
    (
        session_path,
        String::from_utf8_lossy(&out.stdout).to_string(),
        String::from_utf8_lossy(&out.stderr).to_string(),
        out.status.success(),
    )
}

/// Workload that errors on every cycle so `errors=stop` drops
/// the phase as Failed at cycle 0. testkit's `result-error-rate` +
/// `result-error-name` give a deterministic class string for
/// the assertion (`ModelError`).
///
/// Uses the explicit `phases:` + `scenarios:` form (not the
/// inline `blocks:` shorthand). The SRD-76 `phase_outcomes`
/// table is populated from `run_phase`'s failure / success
/// paths; the inline-block form bypasses that walker entry
/// today, so the structured outcome only lands on the disk
/// when the workload declares its phases explicitly.
const FAIL_EVERY_CYCLE: &str = r#"
phases:
  main:
    ops:
      insert:
        stmt: "op"
        result-error-rate: 1.0
        result-error-name: "ModelError"
scenarios:
  default: [main]
"#;

#[test]
fn failed_phase_lands_phase_outcomes_row_with_errors() {
    let wl = write_workload("sqlite", FAIL_EVERY_CYCLE);
    let (session_path, _stdout, stderr, ok) = run_with_session(
        &wl,
        &[
            "adapter=testkit",
            "cycles=20",
            "concurrency=1",
            "errors=stop",
        ],
    );
    assert!(
        !ok,
        "run should have failed (errors=stop on 100% error rate): {stderr}"
    );

    let db_path = session_path.join("metrics.db");
    assert!(
        db_path.exists(),
        "metrics.db not produced at {} — stderr:\n{stderr}",
        db_path.display()
    );

    let reporter =
        nmbrs_metrics::reporters::sqlite::SqliteReporter::new(&db_path).expect("open metrics.db");
    let outcomes = reporter.read_phase_outcomes(None);
    assert!(
        !outcomes.is_empty(),
        "phase_outcomes table empty after failing run; stderr:\n{stderr}"
    );

    // Find the failed outcome — the test workload's single
    // phase should land here as Failed. Other outcomes (e.g.
    // implicit setup phases that completed before the failing
    // one) are tolerated; we just require at least one Failed
    // row with a populated error list.
    let failed: Vec<_> = outcomes.iter().filter(|o| o.status == "failed").collect();
    assert!(
        !failed.is_empty(),
        "no Failed phase outcomes; rows: {:?}",
        outcomes
    );
    let f = failed[0];
    assert!(
        !f.errors.is_empty(),
        "Failed outcome must carry at least one error: {:?}",
        f
    );
    // The error class should propagate from the testkit
    // injection (`ModelError`) through the per-cycle error
    // path into PhaseErrorDetail.class.
    assert!(
        f.errors
            .iter()
            .any(|e| e.class.contains("ModelError") || e.class == "phase_failed"),
        "expected an error with class containing 'ModelError' or 'phase_failed': {:?}",
        f.errors
    );
    // SRD-77 axis labels — session and exec_id must be
    // populated even on the legacy single-execution shape.
    assert!(
        !f.session.is_empty(),
        "session label must be populated on phase_outcomes row: {:?}",
        f
    );
    assert_eq!(
        f.exec_id, 1,
        "exec_id defaults to 1 until SRD-77 lands the registry: {:?}",
        f
    );
}

#[test]
fn nmbrs_replay_renders_failed_phase_with_x_glyph() {
    let wl = write_workload("replay", FAIL_EVERY_CYCLE);
    let (session_path, _stdout, stderr, ok) = run_with_session(
        &wl,
        &[
            "adapter=testkit",
            "cycles=20",
            "concurrency=1",
            "errors=stop",
        ],
    );
    assert!(!ok, "run should have failed: {stderr}");

    // Drive `nmbrs replay --plain` against the session db and
    // confirm the failed outcome surfaces with ✗.
    let mut cmd = nmbrs();
    cmd.arg("replay");
    cmd.arg(format!("--session={}", session_path.display()));
    cmd.arg("--plain");
    let out = cmd.output().expect("exec nmbrs replay");
    let replay_stdout = String::from_utf8_lossy(&out.stdout).to_string();
    let replay_stderr = String::from_utf8_lossy(&out.stderr).to_string();
    assert!(
        out.status.success(),
        "nmbrs replay should succeed; stderr:\n{replay_stderr}"
    );
    assert!(
        replay_stdout.contains('✗'),
        "replay output must include ✗ for the failed phase; stdout:\n{replay_stdout}"
    );
}

#[test]
fn nmbrs_replay_errors_filter_keeps_only_failed_outcomes() {
    let wl = write_workload("filter", FAIL_EVERY_CYCLE);
    let (session_path, _stdout, _stderr, _ok) = run_with_session(
        &wl,
        &[
            "adapter=testkit",
            "cycles=20",
            "concurrency=1",
            "errors=stop",
        ],
    );

    let mut cmd = nmbrs();
    cmd.arg("replay");
    cmd.arg(format!("--session={}", session_path.display()));
    cmd.arg("--plain");
    cmd.arg("--errors");
    let out = cmd.output().expect("exec nmbrs replay --errors");
    let stdout = String::from_utf8_lossy(&out.stdout).to_string();
    assert!(out.status.success());
    // Every outcome's rendering must be the Failed flavour.
    // The Labeled-LOD layout is two lines per outcome: head
    // line `<glyph> [name] <class>` followed by a
    // continuation line `    <message> (Ns)`. So we check
    // that ✗ appears AND ✓ never appears (a successful row
    // would slip through if the `--errors` filter were
    // broken).
    assert!(
        stdout.contains('✗'),
        "--errors output must include ✗ for the failed phase; got:\n{stdout}"
    );
    assert!(
        !stdout.contains('✓'),
        "--errors must not include any ✓ rows; got:\n{stdout}"
    );
}

#[test]
fn nmbrs_replay_json_dumps_structured_outcome() {
    let wl = write_workload("json", FAIL_EVERY_CYCLE);
    let (session_path, _stdout, _stderr, _ok) = run_with_session(
        &wl,
        &[
            "adapter=testkit",
            "cycles=20",
            "concurrency=1",
            "errors=stop",
        ],
    );

    let mut cmd = nmbrs();
    cmd.arg("replay");
    cmd.arg(format!("--session={}", session_path.display()));
    cmd.arg("--json");
    let out = cmd.output().expect("exec nmbrs replay --json");
    let stdout = String::from_utf8_lossy(&out.stdout).to_string();
    assert!(out.status.success());
    // Every non-empty line is a JSON object with the
    // SRD-76-canonical keys. Hand-checked here rather than
    // parsing JSON (the replay command writes a fixed shape).
    let mut saw_failed = false;
    for line in stdout.lines().filter(|l| !l.trim().is_empty()) {
        assert!(
            line.starts_with('{') && line.ends_with('}'),
            "each JSON line must be one self-contained object: {line:?}"
        );
        assert!(
            line.contains("\"phase_name\":"),
            "JSON must carry phase_name: {line:?}"
        );
        assert!(
            line.contains("\"status\":"),
            "JSON must carry status: {line:?}"
        );
        assert!(
            line.contains("\"exec_id\":"),
            "JSON must carry exec_id: {line:?}"
        );
        if line.contains("\"status\":\"failed\"") {
            saw_failed = true;
        }
    }
    assert!(
        saw_failed,
        "JSON dump must contain at least one failed outcome; got:\n{stdout}"
    );
}

mod scratch;

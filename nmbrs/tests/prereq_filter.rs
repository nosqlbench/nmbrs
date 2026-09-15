// Copyright 2024-2026 Jonathan Shook
// SPDX-License-Identifier: Apache-2.0

//! SRD-106 — the `phases=` filter's prereq exemption, over
//! `nmbrs/examples/workloads/controls/prereq_filter_smoke.yaml`.
//!
//! Two rules, one boundary:
//! - a `checkpoint: idempotent` phase in a LIVE scope stays in the
//!   walk when the filter doesn't name it (provenance governs it);
//! - scope liveness comes from SELECTED phases only, so an unselected
//!   section's scope is elided whole — prereqs included.
//!
//! Sandbox discipline per `feedback_tests_no_project_root`.

use std::path::{Path, PathBuf};
use std::process::Command;

const WORKLOAD: &str = "nmbrs/examples/workloads/controls/prereq_filter_smoke.yaml";

struct Sandbox {
    dir: PathBuf,
}

impl Sandbox {
    fn new(tag: &str) -> Self {
        let nanos = std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .unwrap()
            .as_nanos();
        let dir =
            std::env::temp_dir().join(format!("nmbrs-prereq-{tag}-{}-{nanos}", std::process::id()));
        std::fs::create_dir_all(&dir).expect("create sandbox");
        Self { dir }
    }
}

impl Drop for Sandbox {
    fn drop(&mut self) {
        let _ = std::fs::remove_dir_all(&self.dir);
    }
}

fn run_filtered(tag: &str, phases: &str) -> (String, String, bool) {
    let sandbox = Sandbox::new(tag);
    let workspace_root = Path::new(env!("CARGO_MANIFEST_DIR")).parent().unwrap();
    let workload = workspace_root.join(WORKLOAD);
    let session = sandbox.dir.join("session");
    let mut cmd = Command::new(env!("CARGO_BIN_EXE_nmbrs"));
    cmd.current_dir(&sandbox.dir)
        .arg("run")
        .arg(format!("workload={}", workload.display()))
        .arg("scenario=default")
        .arg(format!("phases={phases}"))
        .arg("tui=off")
        .arg("--session-path")
        .arg(&session);
    let out = cmd.output().expect("run nmbrs");
    let session_log = std::fs::read_to_string(session.join("session.log")).unwrap_or_default();
    let mut evidence = String::from_utf8_lossy(&out.stderr).to_string();
    evidence.push('\n');
    evidence.push_str(&session_log);
    (
        String::from_utf8_lossy(&out.stdout).to_string(),
        evidence,
        out.status.success(),
    )
}

fn ticks(stdout: &str, needle: &str) -> usize {
    stdout.lines().filter(|l| l.trim() == needle).count()
}

/// Selecting `measure_a`: the root-level prereq stays in the walk
/// (exemption), the selected measurement runs, and the sibling
/// section's scope — prereq included — is elided whole (liveness
/// from selected phases only).
#[test]
fn prereq_kept_in_live_scope_and_unselected_section_elided() {
    let (stdout, evidence, ok) = run_filtered("a", "measure_a");
    assert!(ok, "filtered run must complete; evidence:\n{evidence}");
    assert_eq!(
        ticks(&stdout, "PREP_TICK"),
        1,
        "root prereq must run under the exemption"
    );
    assert_eq!(
        ticks(&stdout, "A_TICK"),
        2,
        "the selected measurement must run"
    );
    assert_eq!(
        ticks(&stdout, "B_TICK"),
        0,
        "the unselected measurement must not run"
    );
    assert_eq!(
        ticks(&stdout, "SIDE_PREP_TICK"),
        0,
        "an unselected section must not drag its prereq into execution"
    );
    assert!(
        evidence.contains("kept as an idempotent prerequisite"),
        "the exemption must be announced, never silent; evidence:\n{evidence}"
    );
}

/// Selecting `measure_b`: both prereqs run (root-level, and the
/// sibling scope is now live), the selected measurement runs, the
/// unselected one is skipped.
#[test]
fn selecting_the_section_activates_its_scope_and_prereqs() {
    let (stdout, evidence, ok) = run_filtered("b", "measure_b");
    assert!(ok, "filtered run must complete; evidence:\n{evidence}");
    assert_eq!(ticks(&stdout, "PREP_TICK"), 1, "root prereq runs");
    assert_eq!(
        ticks(&stdout, "SIDE_PREP_TICK"),
        1,
        "the live section's own prereq runs"
    );
    assert_eq!(ticks(&stdout, "B_TICK"), 2, "the selected measurement runs");
    assert_eq!(
        ticks(&stdout, "A_TICK"),
        0,
        "the unselected measurement is skipped"
    );
}

/// No filter: everything runs exactly once — the exemption is inert
/// without a pattern.
#[test]
fn unfiltered_run_is_unchanged() {
    let sandbox = Sandbox::new("nofilter");
    let workspace_root = Path::new(env!("CARGO_MANIFEST_DIR")).parent().unwrap();
    let workload = workspace_root.join(WORKLOAD);
    let session = sandbox.dir.join("session");
    let mut cmd = Command::new(env!("CARGO_BIN_EXE_nmbrs"));
    cmd.current_dir(&sandbox.dir)
        .arg("run")
        .arg(format!("workload={}", workload.display()))
        .arg("scenario=default")
        .arg("tui=off")
        .arg("--session-path")
        .arg(&session);
    let out = cmd.output().expect("run nmbrs");
    let stdout = String::from_utf8_lossy(&out.stdout).to_string();
    assert!(out.status.success());
    assert_eq!(
        (
            ticks(&stdout, "PREP_TICK"),
            ticks(&stdout, "A_TICK"),
            ticks(&stdout, "SIDE_PREP_TICK"),
            ticks(&stdout, "B_TICK")
        ),
        (1, 2, 1, 2),
        "unfiltered traversal runs every phase"
    );
}

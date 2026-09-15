// Copyright 2024-2026 Jonathan Shook
// SPDX-License-Identifier: Apache-2.0

//! SRD-106 D2 — skip-validity over `nmbrs refine`, using
//! `nmbrs/examples/workloads/controls/prereq_filter_smoke.yaml`.
//!
//! Three contracts:
//! - a valid idempotent prereq (Completed+Succeeded prior outcome,
//!   hash unchanged) skips via the DEFERRED hash gate — never the
//!   no-hash fast path;
//! - a hash flip (any param change) re-runs the prereq — stale state
//!   is never trusted;
//! - a phase the `phases=` filter names always runs (selection is
//!   intent to run), while plain unfiltered refine keeps SRD-77's
//!   skip-everything-completed behavior.
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
        let dir = std::env::temp_dir().join(format!(
            "nmbrs-refprereq-{tag}-{}-{nanos}",
            std::process::id()
        ));
        std::fs::create_dir_all(&dir).expect("create sandbox");
        Self { dir }
    }
}

impl Drop for Sandbox {
    fn drop(&mut self) {
        let _ = std::fs::remove_dir_all(&self.dir);
    }
}

fn invoke(verb: &str, sandbox: &Sandbox, extra: &[&str]) -> (String, String, bool) {
    let workspace_root = Path::new(env!("CARGO_MANIFEST_DIR")).parent().unwrap();
    let workload = workspace_root.join(WORKLOAD);
    let session = sandbox.dir.join("session");
    let mut cmd = Command::new(env!("CARGO_BIN_EXE_nmbrs"));
    cmd.current_dir(&sandbox.dir)
        .arg(verb)
        .arg(format!("workload={}", workload.display()))
        .arg("scenario=default")
        .arg("tui=off")
        .arg("--session-path")
        .arg(&session);
    // Refine re-attaches through `resume=<dir>` (the resume_target
    // is what the refine plan loads from); `--session-path` alone
    // would fall through to fresh-session creation and collide.
    if verb == "refine" {
        cmd.arg(format!("resume={}", session.display()));
    }
    for a in extra {
        cmd.arg(a);
    }
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

/// Baseline full run, then `refine phases=measure_a`: the selected
/// measurement re-runs (selection defeats the skip), the valid prereq
/// skips via the deferred hash gate, the unselected section stays
/// elided.
#[test]
fn valid_prereq_skips_and_selection_runs() {
    let sandbox = Sandbox::new("valid");
    let (stdout, evidence, ok) = invoke("run", &sandbox, &[]);
    assert!(ok, "baseline run must complete; evidence:\n{evidence}");
    assert_eq!(ticks(&stdout, "PREP_TICK"), 1, "baseline prep runs");

    let (stdout, evidence, ok) = invoke("refine", &sandbox, &["phases=measure_a"]);
    assert!(ok, "refine must complete; evidence:\n{evidence}");
    assert_eq!(
        ticks(&stdout, "A_TICK"),
        2,
        "the selected measurement must RE-RUN under refine (selection \
         is intent to run)"
    );
    assert_eq!(
        ticks(&stdout, "PREP_TICK"),
        0,
        "the valid prereq must skip (prior completed outcome, hash unchanged)"
    );
    assert_eq!(
        ticks(&stdout, "SIDE_PREP_TICK"),
        0,
        "the unselected section stays elided under refine"
    );
    assert!(
        evidence.contains("prior completed outcome, hash unchanged"),
        "the prereq skip must go through the DEFERRED hash gate; \
         evidence:\n{evidence}"
    );
}

/// A CONSUMED param's change invalidates the prereq's provenance:
/// prep consumes `run_tag`, so flipping it re-runs prep — and the
/// diagnostic names the param (SRD-107 Push 3).
#[test]
fn hash_flip_reruns_the_prereq() {
    let sandbox = Sandbox::new("flip");
    let (_stdout, evidence, ok) = invoke("run", &sandbox, &[]);
    assert!(ok, "baseline run must complete; evidence:\n{evidence}");

    let (stdout, evidence, ok) = invoke("refine", &sandbox, &["phases=measure_a", "run_tag=b"]);
    assert!(ok, "refine must complete; evidence:\n{evidence}");
    assert_eq!(
        ticks(&stdout, "PREP_TICK"),
        1,
        "a consumed-param flip invalidates the prereq's provenance — \
         it must re-run"
    );
    assert_eq!(ticks(&stdout, "A_TICK"), 2, "the selection runs");
    assert!(
        evidence.contains("param 'run_tag' changed"),
        "the re-run diagnostic must NAME the changed param; \
         evidence:\n{evidence}"
    );
}

/// SRD-107's headline: a param the prereq does NOT consume may
/// change freely — `probe_tag` feeds only measure_a, so flipping
/// it leaves prep's provenance intact and the load-shaped phase
/// skips. (Pre-SRD-107, ANY param change re-ran every phase.)
#[test]
fn unconsumed_param_flip_still_skips_the_prereq() {
    let sandbox = Sandbox::new("unrelated");
    let (_stdout, evidence, ok) = invoke("run", &sandbox, &[]);
    assert!(ok, "baseline run must complete; evidence:\n{evidence}");

    let (stdout, evidence, ok) = invoke("refine", &sandbox, &["phases=measure_a", "probe_tag=y"]);
    assert!(ok, "refine must complete; evidence:\n{evidence}");
    assert_eq!(
        ticks(&stdout, "PREP_TICK"),
        0,
        "an unconsumed param's change must NOT invalidate the prereq; \
         evidence:\n{evidence}"
    );
    assert_eq!(
        ticks(&stdout, "A_TICK"),
        2,
        "the selected measurement re-runs (selection is intent to run)"
    );
    assert!(
        evidence.contains("prior completed outcome, hash unchanged"),
        "the prereq skip still routes through the hash gate; \
         evidence:\n{evidence}"
    );
}

/// Unfiltered refine keeps SRD-77 semantics: everything with a prior
/// completed outcome skips — measurements via the fast path, prereqs
/// via the hash gate.
#[test]
fn unfiltered_refine_skips_everything_completed() {
    let sandbox = Sandbox::new("plain");
    let (_stdout, evidence, ok) = invoke("run", &sandbox, &[]);
    assert!(ok, "baseline run must complete; evidence:\n{evidence}");

    let (stdout, evidence, ok) = invoke("refine", &sandbox, &[]);
    assert!(ok, "plain refine must complete; evidence:\n{evidence}");
    assert_eq!(
        (
            ticks(&stdout, "PREP_TICK"),
            ticks(&stdout, "A_TICK"),
            ticks(&stdout, "SIDE_PREP_TICK"),
            ticks(&stdout, "B_TICK")
        ),
        (0, 0, 0, 0),
        "SRD-77: a fully-completed session refines to no re-work"
    );
}

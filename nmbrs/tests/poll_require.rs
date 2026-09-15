// Copyright 2024-2026 Jonathan Shook
// SPDX-License-Identifier: Apache-2.0

//! SRD-75 (C5) — phase-poll `require:` strict-gate coverage over
//! `nmbrs/examples/workloads/controls/poll_require_smoke.yaml`.
//!
//! A gate whose `until:` reads live metrics must not trust the
//! predicate while a `require:` selector is unresolved (an
//! unregistered family reads 0.0 silently); past the one-interval
//! grace window an unresolved selector is a hard `poll_require`
//! failure — loud and immediate, never a spurious pass or a
//! hang-to-timeout.
//!
//! Sandbox discipline per `feedback_tests_no_project_root`: cwd is a
//! throwaway dir and the session lands under `--session-path`.

use std::path::{Path, PathBuf};
use std::process::Command;

const WORKLOAD: &str = "nmbrs/examples/workloads/controls/poll_require_smoke.yaml";

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
            "nmbrs-pollreq-{tag}-{}-{nanos}",
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

fn run_scenario(scenario: &str) -> (String, String, bool, PathBuf, Sandbox) {
    let sandbox = Sandbox::new(scenario);
    let workspace_root = Path::new(env!("CARGO_MANIFEST_DIR")).parent().unwrap();
    let workload = workspace_root.join(WORKLOAD);
    let session = sandbox.dir.join("session");
    let mut cmd = Command::new(env!("CARGO_BIN_EXE_nmbrs"));
    cmd.current_dir(&sandbox.dir)
        .arg("run")
        .arg(format!("workload={}", workload.display()))
        .arg(format!("scenario={scenario}"))
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
        session,
        sandbox,
    )
}

/// A selector naming the phase's own instrument family resolves within
/// the grace window; the gate completes cleanly (exit 0) after the
/// predicate turns true.
#[test]
fn resolving_require_lets_the_gate_complete() {
    let (stdout, evidence, ok, _session, _sb) = run_scenario("gate_resolves");
    assert!(
        ok,
        "a resolving require must let the gate complete; evidence:\n{evidence}"
    );
    assert!(
        stdout.lines().any(|l| l.trim() == "GATE_OK_TICK"),
        "the gate's op must have run at least once"
    );
    assert!(
        !evidence.contains("[poll_require]"),
        "no poll_require failure on the resolving path; evidence:\n{evidence}"
    );
}

/// A selector that can never resolve fails the phase with class
/// `poll_require` right after the grace window — well before the 10s
/// `timeout_ms` — with the failing selector named in the evidence.
#[test]
fn unresolved_require_fails_fast_and_loud() {
    let started = std::time::Instant::now();
    let (_stdout, evidence, ok, session, _sb) = run_scenario("gate_typo");
    let elapsed = started.elapsed();
    assert!(
        !ok,
        "an unresolved require must fail the run; evidence:\n{evidence}"
    );
    assert!(
        evidence.contains("[poll_require]"),
        "the failure must carry the poll_require class; evidence:\n{evidence}"
    );
    assert!(
        evidence.contains("no_such_family_xyz"),
        "the failing selector must be named; evidence:\n{evidence}"
    );
    // Fast: grace is one 200ms interval; 10s timeout must NOT be what
    // ended the phase. Generous bound for slow CI.
    assert!(
        elapsed < std::time::Duration::from_secs(8),
        "poll_require must fire at grace expiry, not hang toward \
         timeout_ms; took {elapsed:?}"
    );

    let conn =
        rusqlite::Connection::open(session.join("metrics.db")).expect("open session metrics.db");
    let status: String = conn
        .query_row(
            "SELECT status FROM phase_outcomes WHERE phase_name = 'gate_bad'",
            [],
            |r| r.get(0),
        )
        .expect("gate_bad outcome row");
    assert_eq!(status, "failed", "an unresolved require fails the phase");
}

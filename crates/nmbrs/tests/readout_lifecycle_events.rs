//! E2E test for SRD-63 Push 9a: every lifecycle event
//! actually fires.
//!
//! Each `EventType` variant has a slot the workload `readouts:`
//! block can bind. Before Push 9a, only `EventType::Update`
//! and `EventType::PhaseEnd` were produced anywhere — the
//! other 7 (`SessionStart` / `SessionEnd` /
//! `PhaseStart` / `EachStart` / `EachEnd` /
//! `ScopeStart` / `ScopeEnd`) parsed bindings cleanly
//! and then silently did nothing at runtime. This test
//! binds the `trace` readout to every event family and
//! asserts the event header lines show up in the run's
//! stderr stream.
//!
//! `trace` emits `event=<slot_name> ...` as its first
//! line, so the assertion is just substring matching
//! against the run's log evidence (stderr + session.log:
//! `tui=off` claims the log-only surface, so in-run readout
//! renders land in session.log and are deliberately
//! suppressed from the console).

use std::path::PathBuf;
use std::process::Command;

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
        let path = std::env::temp_dir().join(format!(
            "nmbrs-lifecycle-events-{pid}-{nanos}-{}",
            scratch::seq()
        ));
        std::fs::create_dir_all(&path).expect("create tempdir");
        Self { path }
    }
    fn path(&self) -> &std::path::Path {
        &self.path
    }
}
impl Drop for TempDir {
    fn drop(&mut self) {
        let _ = std::fs::remove_dir_all(&self.path);
    }
}

fn run_workload(yaml: &str) -> String {
    let dir = TempDir::new();
    let yaml_path = dir.path().join("wl.yaml");
    std::fs::write(&yaml_path, yaml).expect("write workload yaml");

    let session_path = dir.path().join("session");
    let mut cmd = Command::new(nmbrs_binary());
    let workspace_root = std::path::Path::new(env!("CARGO_MANIFEST_DIR"))
        .parent()
        .unwrap()
        .parent()
        .expect("workspace root");
    cmd.current_dir(workspace_root);
    cmd.arg("run");
    cmd.arg(format!("workload={}", yaml_path.display()));
    cmd.arg("--session-path");
    cmd.arg(&session_path);
    cmd.arg("tui=off");
    cmd.arg("adapter=stdout");
    let output = cmd.output().expect("run nmbrs");
    let session_log = std::fs::read_to_string(session_path.join("session.log")).unwrap_or_default();
    format!("{}\n{session_log}", String::from_utf8_lossy(&output.stderr))
}

#[test]
fn session_start_and_end_fire_when_bound() {
    let stderr = run_workload(
        r#"
readouts:
  on_session_start: trace
  on_session_end: trace

scenarios:
  default: [run]

phases:
  run:
    adapter: stdout
    cycles: 2
    bindings: |
      c := (cycle)
    ops:
      out: { stmt: "x={c}" }
"#,
    );
    assert!(
        stderr.contains("event=on_session_start"),
        "session_start trace missing: {stderr}"
    );
    assert!(
        stderr.contains("event=on_session_end"),
        "session_end trace missing: {stderr}"
    );
}

#[test]
fn phase_start_fires_when_bound() {
    let stderr = run_workload(
        r#"
readouts:
  on_phase_start: trace

scenarios:
  default: [run]

phases:
  run:
    adapter: stdout
    cycles: 2
    bindings: |
      c := (cycle)
    ops:
      out: { stmt: "x={c}" }
"#,
    );
    assert!(
        stderr.contains("event=on_phase_start"),
        "phase_start trace missing: {stderr}"
    );
}

#[test]
fn each_start_and_end_fire_when_bound() {
    let stderr = run_workload(
        r#"
readouts:
  each_*: trace

scenarios:
  default:
    - for_each: "p in alpha, beta"
      phases: [run]

phases:
  run:
    adapter: stdout
    cycles: 1
    bindings: |
      c := (cycle)
    ops:
      out: { stmt: "p={p}/c={c}" }
"#,
    );
    assert!(
        stderr.contains("event=on_each_start"),
        "each_start trace missing: {stderr}"
    );
    assert!(
        stderr.contains("event=on_each_end"),
        "each_end trace missing: {stderr}"
    );
}

#[test]
fn scope_start_and_end_fire_when_bound_to_do_loop() {
    let stderr = run_workload(
        r#"
params:
  iter_count: "3"

readouts:
  scope_*: trace

scenarios:
  default:
    - do_while: "{i} < {iter_count}"
      counter: i
      phases: [run]

phases:
  run:
    adapter: stdout
    cycles: 1
    bindings: |
      c := (cycle)
    ops:
      out: { stmt: "i={i}/c={c}" }
"#,
    );
    assert!(
        stderr.contains("event=on_scope_start"),
        "scope_start trace missing: {stderr}"
    );
    assert!(
        stderr.contains("event=on_scope_end"),
        "scope_end trace missing: {stderr}"
    );
}

#[test]
fn universal_wildcard_binds_every_event() {
    let stderr = run_workload(
        r#"
readouts:
  "*": trace

scenarios:
  default:
    - for_each: "p in 'alpha'"
      phases: [run]

phases:
  run:
    adapter: stdout
    cycles: 1
    bindings: |
      c := (cycle)
    ops:
      out: { stmt: "p={p}/c={c}" }
"#,
    );
    // Every lifecycle slot the executor reaches in this
    // workload should appear at least once.
    for slot in [
        "on_session_start",
        "on_session_end",
        "on_phase_start",
        "on_phase_end",
        "on_each_start",
        "on_each_end",
    ] {
        assert!(
            stderr.contains(&format!("event={slot}")),
            "wildcard bind missing slot {slot}: {stderr}"
        );
    }
}

mod scratch;

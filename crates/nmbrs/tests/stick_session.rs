// Copyright 2024-2026 Jonathan Shook
// SPDX-License-Identifier: Apache-2.0

//! SRD-106 Part 3 — `stick_session` e2e.
//!
//! Contracts:
//! - a second bare run of a `stick_session: true` workload
//!   re-attaches to `sessions/latest`, layers a new execution,
//!   and announces it — the `session_notice` line renders ahead
//!   of any phase event and names the `--session new` override;
//! - explicit session selection (`--session-name`) defeats
//!   stick — fresh session, no announcement;
//! - the `--session new` bare token defeats stick the same way;
//! - a first run (nothing to re-attach to) stays silent.
//!
//! Sandbox discipline per `feedback_tests_no_project_root`:
//! every invocation runs with the sandbox as cwd, so the
//! `sessions/` root (and its `latest` symlink) lands inside it.

use std::path::PathBuf;
use std::process::Command;

const WORKLOAD: &str = r#"stick_session: true
phases:
  ping:
    cycles: 1
    ops:
      t:
        stmt: "PING"
"#;

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
            std::env::temp_dir().join(format!("nmbrs-stick-{tag}-{}-{nanos}", std::process::id()));
        std::fs::create_dir_all(&dir).expect("create sandbox");
        std::fs::write(dir.join("stick.yaml"), WORKLOAD).expect("write workload");
        Self { dir }
    }

    fn invoke(&self, extra: &[&str]) -> (String, bool) {
        let mut cmd = Command::new(env!("CARGO_BIN_EXE_nmbrs"));
        cmd.current_dir(&self.dir)
            .arg("run")
            .arg("workload=stick.yaml")
            .arg("tui=off");
        for a in extra {
            cmd.arg(a);
        }
        let out = cmd.output().expect("run nmbrs");
        (
            String::from_utf8_lossy(&out.stderr).to_string(),
            out.status.success(),
        )
    }

    fn latest_log(&self) -> String {
        std::fs::read_to_string(self.dir.join("sessions").join("latest").join("session.log"))
            .unwrap_or_default()
    }

    fn session_dirs(&self) -> usize {
        std::fs::read_dir(self.dir.join("sessions"))
            .map(|rd| {
                rd.filter_map(Result::ok)
                    .filter(|e| e.path().is_dir() && !e.path().is_symlink())
                    .count()
            })
            .unwrap_or(0)
    }
}

impl Drop for Sandbox {
    fn drop(&mut self) {
        let _ = std::fs::remove_dir_all(&self.dir);
    }
}

/// Second bare run: re-attach + announce. The notice renders
/// ahead of any phase event and names the override.
#[test]
fn second_run_reattaches_and_announces_first() {
    let sandbox = Sandbox::new("announce");
    let (err, ok) = sandbox.invoke(&[]);
    assert!(ok, "first run must complete: {err}");
    assert!(
        !sandbox.latest_log().contains("sticky session"),
        "a first run has nothing to re-attach to — no announcement"
    );

    let (err, ok) = sandbox.invoke(&[]);
    assert!(ok, "second run must complete: {err}");
    assert_eq!(
        sandbox.session_dirs(),
        1,
        "stick must re-attach, not create a second session"
    );

    let log = sandbox.latest_log();
    assert!(
        log.contains("pass --session new to start fresh"),
        "the notice must name the copy-pasteable override; log:\n{log}"
    );
    // session.log is cumulative across invocations, so the first
    // invocation's phase events legitimately precede the notice.
    // The ordering claim is about the RE-ATTACHED invocation: its
    // phase handling (here, the refine skip of the completed
    // phase) must come after the announcement.
    let notice_at = log
        .find("sticky session: re-attached to")
        .expect("the session_notice line must land in session.log");
    assert!(
        log[notice_at..].contains("phase 'ping'"),
        "the notice must render ahead of the re-attached invocation's \
         phase events; log after notice:\n{}",
        &log[notice_at..]
    );
}

/// Explicit session selection wins outright — fresh named
/// session, no announcement.
#[test]
fn explicit_session_selection_defeats_stick() {
    let sandbox = Sandbox::new("explicit");
    let (_, ok) = sandbox.invoke(&[]);
    assert!(ok);

    let (err, ok) = sandbox.invoke(&["--session-name", "named_b"]);
    assert!(ok, "named run must complete: {err}");
    assert_eq!(
        sandbox.session_dirs(),
        2,
        "an explicit --session-name must produce its own session"
    );
    let named_log = std::fs::read_to_string(
        sandbox
            .dir
            .join("sessions")
            .join("named_b")
            .join("session.log"),
    )
    .unwrap_or_default();
    assert!(
        !named_log.contains("sticky session"),
        "explicit selection must not announce a stick re-attach"
    );
}

/// `--session new` forces a fresh auto-named session.
#[test]
fn session_new_token_defeats_stick() {
    let sandbox = Sandbox::new("new");
    let (_, ok) = sandbox.invoke(&[]);
    assert!(ok);
    // Auto-named sessions timestamp at second resolution; a
    // same-second rerun would collide as a fresh session. One
    // tick of patience keeps the token's behavior observable.
    std::thread::sleep(std::time::Duration::from_millis(1100));

    let (err, ok) = sandbox.invoke(&["--session", "new"]);
    assert!(ok, "--session new run must complete: {err}");
    assert_eq!(
        sandbox.session_dirs(),
        2,
        "--session new must start a fresh session, ignoring stick"
    );
    assert!(
        !sandbox.latest_log().contains("sticky session"),
        "--session new must not announce a re-attach"
    );
}

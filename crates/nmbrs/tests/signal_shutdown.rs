// Copyright 2024-2026 Jonathan Shook
// SPDX-License-Identifier: Apache-2.0

//! SRD-93 M7 / stage 6 — the detached-console signal contract, end to
//! end against the real binary. A detached session has no console to
//! type Ctrl-C into; `kill -TERM` must drive the same three-rung
//! ladder, and the force-exit rung must work from the dedicated
//! signal-dispatch thread even while ops are parked in the adapter.
//!
//! Two spawned-process cases:
//! 1. one SIGTERM → level-1 graceful: the run drains and the shutdown
//!    ceremony completes — WAL consolidated into a self-contained db
//!    (`user_version = 2`, no `-wal` sidecar) with the execution row's
//!    disposition stamped;
//! 2. three SIGTERMs while an op sleeps 60 s in the adapter → the
//!    ladder climbs graceful → cancel-ops → force-exit, and the
//!    process exits `143` (`128 + SIGTERM`) without waiting for the
//!    op — the Motivation-§5 shape, minus the wedged runtime the
//!    in-process harness can't fake.

// The contract under test IS Unix signal delivery — there is no
// Windows equivalent of `kill -TERM` semantics to exercise.
#![cfg(unix)]

use std::path::{Path, PathBuf};
use std::process::{Child, Command, Stdio};
use std::time::{Duration, Instant};

/// A self-contained sandbox under the project's redirected `TMPDIR`
/// (`target/test-tmp`), removed on drop.
struct Sandbox {
    path: PathBuf,
}
impl Sandbox {
    fn new(tag: &str) -> Self {
        let pid = std::process::id();
        let nanos = std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .unwrap()
            .as_nanos();
        let path = std::env::temp_dir().join(format!("nmbrs-signal-{tag}-{pid}-{nanos}"));
        std::fs::create_dir_all(&path).expect("create sandbox");
        Self { path }
    }
}
impl Drop for Sandbox {
    fn drop(&mut self) {
        let _ = std::fs::remove_dir_all(&self.path);
    }
}

/// Spawn `nmbrs run` detached-style (no tty, output discarded) against
/// a sandboxed session dir, and wait until the session's metrics db
/// exists — the reporter opens it in the same setup pass that arms the
/// ladder, so db-present ≈ signal-ready (plus a settle margin).
fn spawn_run(session: &Path, extra: &[&str]) -> Child {
    let mut cmd = Command::new(env!("CARGO_BIN_EXE_nmbrs"));
    cmd.arg("run")
        .args(extra)
        .arg("--session-path")
        .arg(session)
        .stdin(Stdio::null())
        .stdout(Stdio::null())
        .stderr(Stdio::null());
    let child = cmd.spawn().expect("spawn nmbrs run");

    let db = session.join("metrics.db");
    let deadline = Instant::now() + Duration::from_secs(60);
    while !db.exists() {
        assert!(
            Instant::now() < deadline,
            "session db never appeared at {}",
            db.display()
        );
        std::thread::sleep(Duration::from_millis(50));
    }
    // Settle: db creation and ladder arming are the same setup pass,
    // but give the arming store a beat.
    std::thread::sleep(Duration::from_secs(2));
    child
}

fn send_sigterm(child: &Child) {
    let rc = unsafe { libc::kill(child.id() as libc::pid_t, libc::SIGTERM) };
    assert_eq!(rc, 0, "kill -TERM failed");
}

/// Wait for exit with a hard cap; SIGKILL + fail on timeout.
fn wait_up_to(child: &mut Child, cap: Duration) -> std::process::ExitStatus {
    let deadline = Instant::now() + cap;
    loop {
        if let Some(status) = child.try_wait().expect("try_wait") {
            return status;
        }
        if Instant::now() >= deadline {
            let _ = child.kill();
            let _ = child.wait();
            panic!("nmbrs run did not exit within {cap:?}");
        }
        std::thread::sleep(Duration::from_millis(100));
    }
}

#[test]
fn sigterm_drives_graceful_shutdown_with_consolidated_db() {
    let sb = Sandbox::new("graceful");
    let session = sb.path.join("s");

    // Long-running by cycle count so the signal, not completion, ends
    // the run; instant ops so the drain is prompt.
    let mut child = spawn_run(
        &session,
        &[
            "op=t-{cycle}",
            "cycles=1000000",
            "rate=20",
            "adapter=stdout",
        ],
    );

    send_sigterm(&child);
    let status = wait_up_to(&mut child, Duration::from_secs(90));

    // Level-1 graceful is not a force-exit: the process must end by
    // its own normal exit path, not the 143 hard floor.
    assert_ne!(
        status.code(),
        Some(143),
        "graceful TERM must not force-exit"
    );

    // The shutdown ceremony ran: WAL merged into a self-contained db
    // (journal_mode=DELETE drops the sidecars), read indexes built and
    // stamped, and the execution row closed with a disposition.
    let db = session.join("metrics.db");
    assert!(db.exists(), "metrics.db missing after graceful shutdown");
    assert!(
        !session.join("metrics.db-wal").exists(),
        "-wal sidecar still present: WAL consolidation did not run"
    );
    let conn =
        rusqlite::Connection::open_with_flags(&db, rusqlite::OpenFlags::SQLITE_OPEN_READ_ONLY)
            .expect("open consolidated db read-only");
    let version: i64 = conn
        .query_row("PRAGMA user_version", [], |r| r.get(0))
        .expect("read user_version");
    assert_eq!(
        version, 4,
        "shutdown must leave the db at v2-tables + indexes (SRD-93 ladder)"
    );
    let disposition: Option<String> = conn
        .query_row(
            "SELECT disposition FROM executions WHERE verb != 'pending' LIMIT 1",
            [],
            |r| r.get(0),
        )
        .expect("read execution disposition");
    assert!(
        disposition.is_some(),
        "graceful shutdown must stamp the execution disposition"
    );
}

/// Workload whose every op parks in the adapter for 60 s
/// (`result-latency` is an op-level field, not a CLI param), so the
/// cooperative drain cannot finish between rungs — the ladder, not
/// run completion, must decide the exit.
const PARKED_OP_YAML: &str = r#"
blocks:
  main:
    ops:
      park:
        stmt: "parked-{cycle}"
        result-latency: 60000
"#;

#[test]
fn three_sigterms_force_exit_143_past_a_parked_op() {
    let sb = Sandbox::new("force");
    let session = sb.path.join("s");

    let wl = sb.path.join("parked.yaml");
    std::fs::write(&wl, PARKED_OP_YAML).expect("write workload");

    let mut child = spawn_run(
        &session,
        &[
            &format!("workload={}", wl.display()),
            "cycles=1000",
            "adapter=testkit",
        ],
    );

    // One rung per signal: graceful → cancel-ops → force-exit. The
    // gaps only need to outrun signal coalescing (the dispatcher
    // consumes each in microseconds), while staying far inside the
    // 60 s the parked op would take to drain cooperatively.
    send_sigterm(&child);
    std::thread::sleep(Duration::from_millis(300));
    send_sigterm(&child);
    std::thread::sleep(Duration::from_millis(50));
    send_sigterm(&child);

    let status = wait_up_to(&mut child, Duration::from_secs(30));
    assert_eq!(
        status.code(),
        Some(143),
        "third SIGTERM past the cancel rung must force-exit 128+15"
    );
}

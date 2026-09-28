// Copyright 2024-2026 Jonathan Shook
// SPDX-License-Identifier: Apache-2.0

//! A read-only command must not repoint `sessions/latest`.
//!
//! `--session` used to be applied by a startup hook that REPOINTED the
//! `sessions/latest` symlink at whatever was named. Every read-side command
//! defaulting to `sessions/latest/metrics.db` then targeted the right session for
//! free — at the cost of a read mutating shared state. After
//! `nmbrs metrics query … --session=sessions/old`, `latest` pointed at `old`, so a
//! later bare `nmbrs report` or a `--resume-latest` silently operated on `old`
//! rather than the newest real run. It also only worked for sessions under
//! `sessions/`, making `--session=/tmp/x` behave differently from
//! `--session=sessions/x` for no visible reason.
//!
//! These tests pin both halves of the replacement: reads resolve the named
//! session locally and leave the link alone, and the commands that own `latest`
//! still claim it.
//!
//! The same startup hook also PURGED aged-out session directories on every
//! invocation, so a read could delete data outright — the destructive counterpart
//! of the symlink rewrite. Retiring old sessions now happens only for commands
//! that create one, which is the only moment the count grows.

use std::path::{Path, PathBuf};
use std::process::Command;

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
        let path = std::env::temp_dir().join(format!("nmbrs-latest-{tag}-{pid}-{nanos}"));
        std::fs::create_dir_all(&path).expect("create sandbox");
        std::fs::write(
            path.join("w.yaml"),
            "params:\n  adapter: stdout\nops:\n  g:\n    stmt: \"ok\"\n",
        )
        .expect("write workload");
        Self { path }
    }

    fn nmbrs(&self, args: &[&str]) -> String {
        let out = Command::new(env!("CARGO_BIN_EXE_nmbrs"))
            .args(args)
            .current_dir(&self.path)
            .output()
            .expect("spawn nmbrs");
        format!(
            "{}{}",
            String::from_utf8_lossy(&out.stdout),
            String::from_utf8_lossy(&out.stderr)
        )
    }

    fn run(&self) -> String {
        self.nmbrs(&["run", "workload=w.yaml", "adapter=stdout", "cycles=2"])
    }

    fn latest(&self) -> PathBuf {
        let t = std::fs::read_link(self.path.join("sessions").join("latest"))
            .expect("sessions/latest should be a symlink");
        // Unix symlinks carry the relative session id; the Windows
        // junction fallback reads back an absolute target. Normalize
        // to the session id (final component) for comparisons.
        PathBuf::from(t.file_name().expect("latest target has a final component"))
    }

    /// Real session directories only. `sessions/` also holds the per-artifact
    /// convenience symlinks (`latest`, `session.log`, `metrics.db`), so filtering
    /// by name alone counts links as sessions.
    fn session_dirs(&self) -> Vec<String> {
        let mut v: Vec<String> = std::fs::read_dir(self.path.join("sessions"))
            .expect("read sessions/")
            .filter_map(|e| e.ok())
            .filter(|e| {
                e.file_type()
                    .map(|t| t.is_dir() && !t.is_symlink())
                    .unwrap_or(false)
            })
            .map(|e| e.file_name().to_string_lossy().into_owned())
            .collect();
        v.sort();
        v
    }
}

impl Drop for Sandbox {
    fn drop(&mut self) {
        let _ = std::fs::remove_dir_all(&self.path);
    }
}

/// The session auto-id is `{scenario}_{timestamp}` at SECOND granularity, so two
/// runs inside one second land in the same directory and the second exits on the
/// reuse policy. Wait past the tick between runs.
fn tick() {
    std::thread::sleep(std::time::Duration::from_millis(1100));
}

#[test]
fn a_read_command_does_not_repoint_latest() {
    let sb = Sandbox::new("read");
    sb.run();
    tick();
    let second = sb.run();

    let dirs = sb.session_dirs();
    assert_eq!(
        dirs.len(),
        2,
        "expected two sessions, got {dirs:?}:\n{second}"
    );
    let newest = sb.latest();
    let older = dirs
        .iter()
        .find(|d| Path::new(d) != newest.as_path())
        .expect("one session that is not the latest");

    // Read the OLDER session by name — the case that used to hijack the link.
    let out = sb.nmbrs(&[
        "metrics",
        "query",
        "cycles_total",
        &format!("--session=sessions/{older}"),
    ]);

    // Not vacuous: the read must actually have targeted the older session. Every
    // series carries a `session="…"` label, so the output names the db it read.
    assert!(
        out.contains(&format!("session=\"{older}\"")),
        "the read must resolve the NAMED session (expected {older}); got:\n{out}"
    );
    assert_eq!(
        sb.latest(),
        newest,
        "a read-only command must leave sessions/latest alone; got:\n{out}"
    );
}

/// The other half: removing the hook must not stop the commands that legitimately
/// own `latest` from claiming it. `Session::new_with_args` does this for a fresh
/// run, including when the session is named explicitly.
#[test]
fn a_write_still_claims_latest_including_a_named_session() {
    let sb = Sandbox::new("write");
    sb.run();
    let auto = sb.latest();
    assert!(
        auto.to_string_lossy().starts_with("default_"),
        "a bare run claims latest for its auto-id, got {auto:?}"
    );

    let out = sb.nmbrs(&[
        "run",
        "workload=w.yaml",
        "adapter=stdout",
        "cycles=2",
        "--session-name=mysess",
    ]);
    assert_eq!(
        sb.latest(),
        Path::new("mysess"),
        "a run with an explicit session name must claim latest; got:\n{out}"
    );
    assert!(
        sb.path.join("sessions/mysess/metrics.db").exists(),
        "the named session must hold the run's artifacts; got:\n{out}"
    );
}

/// The consequence that made this worth fixing: an intervening read must not
/// change what `--resume-latest` resumes.
#[test]
fn a_read_does_not_change_what_resume_latest_resumes() {
    let sb = Sandbox::new("resume");
    sb.run();
    tick();
    sb.run();

    let dirs = sb.session_dirs();
    let newest = sb.latest();
    let older = dirs
        .iter()
        .find(|d| Path::new(d) != newest.as_path())
        .expect("one non-latest session");

    sb.nmbrs(&[
        "metrics",
        "query",
        "cycles_total",
        &format!("--session=sessions/{older}"),
    ]);
    let out = sb.nmbrs(&[
        "run",
        "workload=w.yaml",
        "adapter=stdout",
        "cycles=2",
        "--resume-latest",
    ]);

    assert_eq!(
        sb.latest(),
        newest,
        "--resume-latest must still target the newest real run after a read; \
         got:\n{out}"
    );
}

/// A read-only command must not DELETE sessions.
///
/// The startup cleanup keeps the `--session-keep` most recent directories and
/// removes the rest. It ran unconditionally, so merely reporting on a session
/// could destroy older ones.
///
/// Three sessions are needed, not two: the purge excludes whatever `latest` points
/// at, so with two directories and `--session-keep=1` the effective count is 1 and
/// nothing would be purged even under the old behaviour — the test would pass
/// vacuously. With three, one non-latest directory is over the cap and the old
/// code would delete it.
///
/// The flag spelling matters too: `--session-keep` is canonical (`--sessions-max`
/// is now an accepted alias, but was documented-only and parsed nowhere when this
/// was written), and an unrecognised flag would make this vacuous.
#[test]
fn a_read_command_does_not_purge_sessions() {
    let sb = Sandbox::new("purge");
    sb.run();
    tick();
    sb.run();
    tick();
    sb.run();
    let before = sb.session_dirs();
    assert_eq!(before.len(), 3, "expected three sessions, got {before:?}");
    let oldest = before[0].clone(); // sorted by name ⇒ by timestamp

    let out = sb.nmbrs(&["metrics", "query", "cycles_total", "--session-keep=1"]);

    assert!(
        sb.session_dirs().contains(&oldest),
        "a read must not purge the oldest session under an eager --session-keep; \
         got {:?}:\n{out}",
        sb.session_dirs()
    );
    assert_eq!(
        sb.session_dirs(),
        before,
        "a read must not purge ANY session; got:\n{out}"
    );
}

/// The other half: a session-creating command still retires old ones, so bounding
/// disk use did not get lost in the process.
#[test]
fn a_write_still_purges_under_the_cap() {
    let sb = Sandbox::new("purge-write");
    sb.run();
    tick();
    sb.run();
    tick();
    sb.run();
    let before = sb.session_dirs();
    assert_eq!(before.len(), 3, "expected three sessions, got {before:?}");
    let oldest = before[0].clone();

    tick();
    let out = sb.nmbrs(&[
        "run",
        "workload=w.yaml",
        "adapter=stdout",
        "cycles=2",
        "--session-keep=1",
    ]);
    let after = sb.session_dirs();
    assert!(
        !after.contains(&oldest),
        "a writing command must still retire the oldest session under the cap; \
         got {after:?}:\n{out}"
    );
}

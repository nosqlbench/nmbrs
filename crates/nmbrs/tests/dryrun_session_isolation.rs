// Copyright 2024-2026 Jonathan Shook
// SPDX-License-Identifier: Apache-2.0

//! A dry-run must not write into another session's directory.
//!
//! A dry-run is resume-inert (SRD-44): it deliberately does not claim
//! `sessions/latest`, so that a later `--resume-latest` resumes the last REAL
//! run rather than a throwaway. `args_request_dryrun` — the predicate that
//! decides this — was unit-tested, but nothing tested the consequence, and the
//! post-run artifact path had its own session resolution that fell back to
//! `latest_session_dir()` when no `--session*` flag was passed. For a normal run
//! that fallback is right by accident (the run just claimed `latest`); for a
//! dry-run it resolved to the PREVIOUS REAL SESSION, so the dry-run wrote
//! `scenario_tree.txt` into it, rewrote its `summary.md`, and pointed
//! `auto_render_plots` / `auto_inject_details` at its `metrics.db`.
//!
//! These tests drive the real binary in a sandbox cwd, because the defect lived
//! in how the CLI resolved a directory — not in anything reachable from a unit
//! test of the runtime.

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
        let path = std::env::temp_dir().join(format!("nmbrs-dryrun-iso-{tag}-{pid}-{nanos}"));
        std::fs::create_dir_all(&path).expect("create sandbox");
        Self { path }
    }

    /// A minimal stdout workload — no external service, so the run is fast and
    /// hermetic.
    fn write_workload(&self) -> PathBuf {
        let wl = self.path.join("tiny.yaml");
        std::fs::write(
            &wl,
            "params:\n  adapter: stdout\nops:\n  g:\n    stmt: \"ok\"\n",
        )
        .expect("write workload");
        wl
    }

    fn nmbrs(&self, args: &[&str]) -> String {
        let out = Command::new(env!("CARGO_BIN_EXE_nmbrs"))
            .args(args)
            .current_dir(&self.path)
            .output()
            .expect("spawn nmbrs");
        format!(
            "[exit {:?}]\n{}{}",
            out.status.code(),
            String::from_utf8_lossy(&out.stdout),
            String::from_utf8_lossy(&out.stderr)
        )
    }
}

impl Drop for Sandbox {
    fn drop(&mut self) {
        let _ = std::fs::remove_dir_all(&self.path);
    }
}

/// Fingerprint a directory by (file name, size, mtime) so a rewrite of existing
/// content is caught as surely as a new file.
fn fingerprint(dir: &Path) -> Vec<(String, u64, std::time::SystemTime)> {
    let mut out: Vec<(String, u64, std::time::SystemTime)> = std::fs::read_dir(dir)
        .expect("read session dir")
        .filter_map(|e| e.ok())
        .filter_map(|e| {
            let md = e.metadata().ok()?;
            Some((
                e.file_name().to_string_lossy().into_owned(),
                md.len(),
                md.modified().ok()?,
            ))
        })
        .collect();
    out.sort();
    out
}

#[test]
fn dryrun_does_not_write_into_the_previous_real_session() {
    let sb = Sandbox::new("isolation");
    sb.write_workload();

    // A real run first: it claims `latest` and writes its artifacts.
    let first = sb.nmbrs(&["run", "workload=tiny.yaml", "adapter=stdout", "cycles=2"]);
    let latest = sb.path.join("sessions").join("latest");
    assert!(
        latest.exists(),
        "a normal run must claim sessions/latest; got:\n{first}"
    );
    let real_target = std::fs::read_link(&latest).expect("latest is a symlink");
    let before = fingerprint(&latest);
    assert!(
        before.iter().any(|(n, _, _)| n == "scenario_tree.txt"),
        "a normal run must persist its scenario tree: {before:?}"
    );

    // The auto session id is `{scenario}_{timestamp}` at SECOND granularity, so
    // two runs inside the same second resolve to the same directory and the
    // second one exits on the reuse policy — which would make this test pass for
    // the wrong reason. Wait past the tick so the dry-run gets its own id.
    std::thread::sleep(std::time::Duration::from_millis(1100));

    // Now a dry-run with no `--session*`: the case that used to resolve onto the
    // run above.
    let dry = sb.nmbrs(&[
        "run",
        "workload=tiny.yaml",
        "adapter=stdout",
        "dryrun=phase",
    ]);

    // Guard against a VACUOUS pass: if the dry-run bailed out before its
    // post-run block, the directory would be untouched for the wrong reason and
    // this test would pass on the broken code too.
    assert!(
        dry.contains("phases:"),
        "the dry-run must actually reach its post-run summary, else this test \
         proves nothing; got:\n{dry}"
    );

    assert_eq!(
        std::fs::read_link(&latest).expect("latest still a symlink"),
        real_target,
        "a dry-run must not re-point sessions/latest; got:\n{dry}"
    );
    assert_eq!(
        fingerprint(&latest),
        before,
        "a dry-run must not add to or rewrite the previous real session's \
         directory; got:\n{dry}"
    );
}

/// The dry-run still reports its outcome — the fix skips only the artifact
/// write, not the summary. Otherwise "don't corrupt the other session" would
/// have been traded for "say nothing at all".
#[test]
fn dryrun_still_prints_its_phase_summary() {
    let sb = Sandbox::new("summary");
    sb.write_workload();
    let out = sb.nmbrs(&[
        "run",
        "workload=tiny.yaml",
        "adapter=stdout",
        "dryrun=phase",
    ]);
    assert!(
        out.contains("phases:"),
        "dry-run must still print its phase summary; got:\n{out}"
    );
    // And it must not name a directory it does not own.
    assert!(
        !out.contains("logs:    sessions/latest"),
        "dry-run must not claim sessions/latest as its log dir; got:\n{out}"
    );
}

// Copyright 2024-2026 Jonathan Shook
// SPDX-License-Identifier: Apache-2.0

//! SRD-64 §12 acceptance test for the `nmbrs report` CLI.
//!
//! Drives the full end-to-end sequence in one integration
//! test so the per-phase pieces (vocab parser, scratch
//! render, anchor resolver, edit primitive, rename) prove
//! they compose correctly:
//!
//! 1. Run a tiny workload with no `report:` block.
//! 2. Render a plot to scratch (no `--add`).
//! 3. `--add` (default root anchor) — workload mutates,
//!    backup at `<wl>.bak` holds the pre-edit content.
//! 4. Same `--add` again — name-collision error names the
//!    site + remediation flags.
//! 5. `--add --replace` — in-place update; backup pair
//!    rotates so `.bak.prev` holds two-edits-back.
//! 6. `nmbrs report rename` — pure metadata edit; backup
//!    rotates again.
//! 7. `--add --contextual auto` — anchor walk picks the
//!    deepest unique scope from session data.
//! 8. `parse_workload` succeeds at every step (ensured
//!    implicitly: every `--add` runs the post-edit roundtrip
//!    parse internally; if it failed the CLI would error
//!    and the assertions below would fail too).
//! 9. Concurrent `--add` lock semantics — two processes
//!    against the same workload serialise on the file lock.
//!
//! The test runs entirely under
//! `target/test-tmp/<name>/` (TMPDIR is redirected by the
//! workspace `.cargo/config.toml`); no project-root logs
//! pollution.

use std::path::{Path, PathBuf};
use std::process::{Command, Stdio};

/// Per-invocation sandbox under `target/test-tmp/`. Workspace
/// `.cargo/config.toml` redirects `TMPDIR`, so
/// `std::env::temp_dir()` is already
/// `<workspace>/target/test-tmp/`. We add our own subdir for
/// per-test isolation.
struct Sandbox {
    root: PathBuf,
    workload: PathBuf,
    session: PathBuf,
}

impl Sandbox {
    fn new(label: &str) -> Self {
        let pid = std::process::id();
        let nanos = std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .unwrap()
            .as_nanos();
        let root =
            std::env::temp_dir().join(format!("nmbrs-srd64-acceptance-{label}-{pid}-{nanos}"));
        std::fs::create_dir_all(&root).expect("sandbox create");
        let workload = root.join("wl.yaml");
        let session = root.join("session");
        Self {
            root,
            workload,
            session,
        }
    }
}

impl Drop for Sandbox {
    fn drop(&mut self) {
        let _ = std::fs::remove_dir_all(&self.root);
    }
}

/// Build a minimal one-phase workload with a top-level
/// comment so we can assert comment preservation across
/// edits later.
fn write_minimal_workload(path: &Path) {
    std::fs::write(
        path,
        concat!(
            "# top-of-file comment preserved across edits\n",
            "phases:\n",
            "  setup:\n",
            "    ops:\n",
            "      step: \"noop\"\n",
        ),
    )
    .unwrap();
}

fn nmbrs_bin() -> Command {
    let mut cmd = Command::new(env!("CARGO_BIN_EXE_nmbrs"));
    let workspace_root = std::path::Path::new(env!("CARGO_MANIFEST_DIR"))
        .parent()
        .unwrap()
        .parent()
        .expect("workspace root");
    cmd.current_dir(workspace_root);
    cmd
}

fn run_workload(sb: &Sandbox) {
    let output = nmbrs_bin()
        .arg("run")
        .arg(&sb.workload)
        .arg("cycles=2")
        .arg("--session-path")
        .arg(&sb.session)
        .output()
        .expect("nmbrs run");
    let stderr = String::from_utf8_lossy(&output.stderr);
    assert!(
        stderr.contains("done") || stderr.contains("setup"),
        "workload run stderr: {stderr}"
    );
}

fn report_cmd(sb: &Sandbox, args: &[&str]) -> std::process::Output {
    let mut cmd = nmbrs_bin();
    cmd.arg("report");
    for a in args {
        cmd.arg(a);
    }
    cmd.arg("--session-path").arg(&sb.session);
    cmd.output().expect("nmbrs report")
}

fn assert_succeeds(out: &std::process::Output, label: &str) {
    if !out.status.success() {
        panic!(
            "{label}: nmbrs report exited {:?}\n--- stderr ---\n{}",
            out.status.code(),
            String::from_utf8_lossy(&out.stderr),
        );
    }
}

fn assert_errors_with(out: &std::process::Output, needle: &str, label: &str) {
    let stderr = String::from_utf8_lossy(&out.stderr);
    assert!(
        !out.status.success(),
        "{label}: expected error exit, got success"
    );
    assert!(
        stderr.contains(needle),
        "{label}: stderr should contain {needle:?}; got:\n{stderr}"
    );
}

#[test]
fn srd64_acceptance_full_flow() {
    let sb = Sandbox::new("full_flow");

    // ── 1. Tiny workload with no report: block. ──
    write_minimal_workload(&sb.workload);
    run_workload(&sb);

    // ── 2. Render to scratch (no --add). ──
    let out = report_cmd(
        &sb,
        &[
            "plot",
            "demo",
            "--over",
            "cycle",
            "--metric",
            "throughput",
            "--label",
            "Demo plot",
        ],
    );
    assert_succeeds(&out, "step 2 (scratch render)");
    let scratch = sb.session.join("scratch/demo.md");
    assert!(
        scratch.exists(),
        "step 2: scratch file should exist at {}",
        scratch.display()
    );

    // Workload byte-identical: no mutation without --add.
    let pre_add = std::fs::read_to_string(&sb.workload).unwrap();
    assert!(
        !pre_add.contains("report:"),
        "step 2: workload must not have been mutated"
    );

    // ── 3. --add (default root anchor) ──
    let out = report_cmd(
        &sb,
        &[
            "plot",
            "demo",
            "--over",
            "cycle",
            "--metric",
            "throughput",
            "--label",
            "Demo plot",
            "--workload",
            sb.workload.to_str().unwrap(),
            "--add",
        ],
    );
    assert_succeeds(&out, "step 3 (--add)");
    let stderr = String::from_utf8_lossy(&out.stderr);
    assert!(
        stderr.contains("anchor: workload root"),
        "step 3: should report root anchor, got: {stderr}"
    );
    assert!(
        stderr.contains("inserted"),
        "step 3: should report 'inserted', got: {stderr}"
    );
    let after_add = std::fs::read_to_string(&sb.workload).unwrap();
    assert!(
        after_add.contains("report:"),
        "step 3: workload should now contain a report: block:\n{after_add}"
    );
    assert!(
        after_add.contains("plot demo"),
        "step 3: workload should contain plot demo"
    );
    // Top-of-file comment preserved.
    assert!(
        after_add.starts_with("# top-of-file comment preserved"),
        "step 3: comment must survive edit"
    );
    // Backup holds pre-edit content.
    let bak = sb.root.join("wl.yaml.bak");
    assert!(bak.exists(), "step 3: .bak must exist");
    let bak_content = std::fs::read_to_string(&bak).unwrap();
    assert!(
        !bak_content.contains("report:"),
        "step 3: .bak should hold the pre-edit (no-report) content"
    );

    // ── 4. Re-add same name without --replace → collision. ──
    let out = report_cmd(
        &sb,
        &[
            "plot",
            "demo",
            "--over",
            "cycle",
            "--metric",
            "throughput",
            "--label",
            "Different",
            "--workload",
            sb.workload.to_str().unwrap(),
            "--add",
        ],
    );
    assert_errors_with(&out, "already defined", "step 4 (collision)");
    let stderr = String::from_utf8_lossy(&out.stderr);
    assert!(
        stderr.contains("--replace"),
        "step 4: error should mention --replace remediation"
    );

    // ── 5. --add --replace → in-place update, backup rotates. ──
    let out = report_cmd(
        &sb,
        &[
            "plot",
            "demo",
            "--over",
            "cycle",
            "--metric",
            "throughput",
            "--label",
            "Updated label",
            "--workload",
            sb.workload.to_str().unwrap(),
            "--add",
            "--replace",
        ],
    );
    assert_succeeds(&out, "step 5 (--replace)");
    let stderr = String::from_utf8_lossy(&out.stderr);
    assert!(
        stderr.contains("replaced"),
        "step 5: should report 'replaced'"
    );
    let after_replace = std::fs::read_to_string(&sb.workload).unwrap();
    assert!(
        after_replace.contains("Updated label"),
        "step 5: new label should be present"
    );
    // Backup pair rotates: .bak holds the just-prior (post-add)
    // content, .bak.prev holds the original (no-report) one.
    let bak_after = std::fs::read_to_string(&bak).unwrap();
    assert!(
        bak_after.contains("Demo plot"),
        "step 5: .bak should hold the just-prior content (post-step-3)"
    );
    let bak_prev = sb.root.join("wl.yaml.bak.prev");
    assert!(
        bak_prev.exists(),
        "step 5: .bak.prev should exist after the second mutation"
    );
    let prev_content = std::fs::read_to_string(&bak_prev).unwrap();
    assert!(
        !prev_content.contains("report:"),
        "step 5: .bak.prev should hold the original (no-report) content"
    );

    // ── 6. nmbrs report rename ──
    let out = nmbrs_bin()
        .arg("report")
        .arg("rename")
        .arg("demo")
        .arg("demo_v2")
        .arg("--workload")
        .arg(&sb.workload)
        .arg("--session-path")
        .arg(&sb.session)
        .output()
        .expect("rename");
    assert_succeeds(&out, "step 6 (rename)");
    let renamed = std::fs::read_to_string(&sb.workload).unwrap();
    assert!(
        renamed.contains("plot demo_v2"),
        "step 6: workload should contain renamed item:\n{renamed}"
    );
    // Original name is gone (modulo the sentinel `plot demo_v2` substring).
    assert!(
        !renamed.contains("plot demo\n"),
        "step 6: original name should be gone (no `plot demo` line)"
    );

    // ── 7. --contextual auto picks phase:setup ──
    let out = report_cmd(
        &sb,
        &[
            "plot",
            "demo_phase",
            "--over",
            "cycle",
            "--workload",
            sb.workload.to_str().unwrap(),
            "--add",
            "--contextual",
            "auto",
            "--dry-run",
        ],
    );
    assert_succeeds(&out, "step 7 (--contextual auto dry-run)");
    let stderr = String::from_utf8_lossy(&out.stderr);
    let stdout = String::from_utf8_lossy(&out.stdout);
    let combined = format!("{stderr}{stdout}");
    assert!(
        combined.contains("phase:setup"),
        "step 7: --contextual auto should pick phase:setup, got:\n{combined}"
    );

    // ── 8. parse_workload succeeds at every step. ──
    // Implicit: every --add ran the post-edit roundtrip parse
    // (else step 3/5/6 would have errored). Re-run a final
    // parse to pin it explicitly.
    let final_yaml = std::fs::read_to_string(&sb.workload).unwrap();
    let _: serde_yaml::Value =
        serde_yaml::from_str(&final_yaml).expect("step 8: final workload must be valid YAML");

    // ── 9. Concurrent --add lock semantics. ──
    // Two processes against the same workload — the lock
    // serialises them. Both should succeed (or one waits and
    // catches up); neither corrupts the file. We launch both
    // with --replace so name collision doesn't fight us.
    let work_a = std::process::Command::new(env!("CARGO_BIN_EXE_nmbrs"))
        .current_dir(
            std::path::Path::new(env!("CARGO_MANIFEST_DIR"))
                .parent()
                .unwrap()
                .parent()
                .expect("workspace root"),
        )
        .args([
            "report",
            "plot",
            "demo_v2",
            "--over",
            "cycle",
            "--label",
            "From A",
            "--add",
            "--replace",
            "--workload",
            sb.workload.to_str().unwrap(),
            "--session-path",
            sb.session.to_str().unwrap(),
        ])
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .spawn()
        .expect("spawn A");
    let work_b = std::process::Command::new(env!("CARGO_BIN_EXE_nmbrs"))
        .current_dir(
            std::path::Path::new(env!("CARGO_MANIFEST_DIR"))
                .parent()
                .unwrap()
                .parent()
                .expect("workspace root"),
        )
        .args([
            "report",
            "plot",
            "demo_v2",
            "--over",
            "cycle",
            "--label",
            "From B",
            "--add",
            "--replace",
            "--workload",
            sb.workload.to_str().unwrap(),
            "--session-path",
            sb.session.to_str().unwrap(),
        ])
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .spawn()
        .expect("spawn B");
    let res_a = work_a.wait_with_output().expect("wait A");
    let res_b = work_b.wait_with_output().expect("wait B");
    // At least one must have succeeded; the other either also
    // succeeded (lock acquired in sequence) or errored with a
    // WouldBlock-ish message after the 5s deadline.
    let a_ok = res_a.status.success();
    let b_ok = res_b.status.success();
    assert!(
        a_ok || b_ok,
        "step 9: at least one concurrent --add must succeed.\nA stderr: {}\nB stderr: {}",
        String::from_utf8_lossy(&res_a.stderr),
        String::from_utf8_lossy(&res_b.stderr)
    );
    // The workload file is still parseable — no half-written
    // content from concurrent races.
    let post_concurrent = std::fs::read_to_string(&sb.workload).unwrap();
    let _: serde_yaml::Value = serde_yaml::from_str(&post_concurrent)
        .expect("step 9: workload must still parse after concurrent edits");
    // Exactly one `plot demo_v2` survives — both writes target
    // the same name with --replace, so the final state has one
    // entry whose label is whichever process won the race.
    let n = post_concurrent.matches("plot demo_v2").count();
    assert_eq!(
        n, 1,
        "step 9: exactly one demo_v2 entry should remain, got {n} in:\n{post_concurrent}"
    );
    assert!(
        post_concurrent.contains("From A") || post_concurrent.contains("From B"),
        "step 9: surviving label should be from one of the two writers"
    );
}

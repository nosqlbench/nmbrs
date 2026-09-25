// Copyright 2024-2026 Jonathan Shook
// SPDX-License-Identifier: Apache-2.0

//! Smoke test for the `nmbrs check` subcommand — the subprocess
//! verification path (`verify_target` → `verify_path` → `run_case`),
//! which re-spawns `nmbrs run` per case and checks its combined output
//! against the workload's `#@`/`verify:` rules.
//!
//! The bundled-example CI gate moved in-process
//! (`example_workloads_in_process.rs`), but `nmbrs check` is a user
//! command that still drives this subprocess path, so it keeps a
//! dedicated end-to-end smoke test. One tiny self-contained workload
//! (not the example tree) — this covers the CLI mechanism, fast.

use std::path::PathBuf;
use std::process::Command;

/// A self-contained sandbox under the project's redirected `TMPDIR`
/// (`target/test-tmp`), removed on drop.
struct Sandbox {
    path: PathBuf,
}
impl Sandbox {
    fn new() -> Self {
        let pid = std::process::id();
        let nanos = std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .unwrap()
            .as_nanos();
        let path =
            std::env::temp_dir().join(format!("nmbrs-check-cli-{pid}-{nanos}-{}", scratch::seq()));
        std::fs::create_dir_all(&path).expect("create sandbox");
        Self { path }
    }
}
impl Drop for Sandbox {
    fn drop(&mut self) {
        let _ = std::fs::remove_dir_all(&self.path);
    }
}

#[test]
fn nmbrs_check_verifies_a_workload_directory() {
    let sb = Sandbox::new();
    let wl_dir = sb.path.join("workloads");
    std::fs::create_dir_all(&wl_dir).unwrap();

    // A minimal bare stdout workload with embedded `#@` rules: two
    // cycles each print a static line; the rules match that op output
    // and the clean phase summary.
    std::fs::write(
        wl_dir.join("smoke.yaml"),
        "params:\n  \
           adapter: stdout\n\
         ops:\n  \
           greet:\n    \
             stmt: \"smoke-ok\"\n\
         #@ run cycles=2\n\
         #@ expect smoke-ok\n\
         #@ expect 0 failed\n",
    )
    .unwrap();

    // `nmbrs check <dir>` walks the dir, re-spawns `nmbrs run` per case
    // (in its own sandbox), and verifies the output. Run from the
    // sandbox so nothing touches the project root.
    let out = Command::new(env!("CARGO_BIN_EXE_nmbrs"))
        .arg("check")
        .arg(&wl_dir)
        .current_dir(&sb.path)
        .output()
        .expect("spawn nmbrs check");

    let stdout = String::from_utf8_lossy(&out.stdout);
    let stderr = String::from_utf8_lossy(&out.stderr);
    assert!(
        out.status.success(),
        "`nmbrs check` should pass.\nstdout:\n{stdout}\nstderr:\n{stderr}"
    );
    assert!(
        stdout.contains("passed"),
        "expected a passing summary line, got:\nstdout:\n{stdout}\nstderr:\n{stderr}"
    );
}

#[test]
fn nmbrs_check_reports_slowest_workloads_for_a_directory() {
    // Two workloads → the end-of-run report ranks them by time taken.
    // (The live active/pending/done status line is TTY-only and absent
    // when stdout is captured; the timing report goes to stdout.)
    let sb = Sandbox::new();
    let wl_dir = sb.path.join("workloads");
    std::fs::create_dir_all(&wl_dir).unwrap();
    for name in ["alpha", "beta"] {
        std::fs::write(
            wl_dir.join(format!("{name}.yaml")),
            "params:\n  adapter: stdout\nops:\n  g:\n    stmt: \"ok\"\n\
             #@ run cycles=1\n#@ expect ok\n",
        )
        .unwrap();
    }

    let out = Command::new(env!("CARGO_BIN_EXE_nmbrs"))
        .arg("check")
        .arg(&wl_dir)
        .current_dir(&sb.path)
        .output()
        .expect("spawn nmbrs check");

    let stdout = String::from_utf8_lossy(&out.stdout);
    assert!(out.status.success(), "should pass:\n{stdout}");
    assert!(
        stdout.contains("top 2 by time taken"),
        "expected a slowest-workloads report, got:\nstdout:\n{stdout}"
    );
    assert!(
        stdout.contains("alpha.yaml") && stdout.contains("beta.yaml"),
        "expected both workloads ranked, got:\nstdout:\n{stdout}"
    );
}

#[test]
fn nmbrs_check_reports_failure_on_a_violated_rule() {
    let sb = Sandbox::new();
    let wl_dir = sb.path.join("workloads");
    std::fs::create_dir_all(&wl_dir).unwrap();

    // Same workload, but an `#@ expect` that the output will NOT match —
    // `nmbrs check` must exit non-zero (the CI-gate contract).
    std::fs::write(
        wl_dir.join("bad.yaml"),
        "params:\n  \
           adapter: stdout\n\
         ops:\n  \
           greet:\n    \
             stmt: \"smoke-ok\"\n\
         #@ run cycles=2\n\
         #@ expect this-string-never-appears\n",
    )
    .unwrap();

    let out = Command::new(env!("CARGO_BIN_EXE_nmbrs"))
        .arg("check")
        .arg(&wl_dir)
        .current_dir(&sb.path)
        .output()
        .expect("spawn nmbrs check");

    assert!(
        !out.status.success(),
        "`nmbrs check` must exit non-zero when a rule is violated.\nstdout:\n{}",
        String::from_utf8_lossy(&out.stdout)
    );
}

#[test]
fn nmbrs_check_unknown_name_suggests_buried_leaf_matches() {
    // A bare stem the operator half-remembers, with the real workload
    // buried three directory levels below cwd. `nmbrs check` must fail to
    // resolve it and offer the deep leaf-segment match as a "did you mean"
    // suggestion (the shared `nmbrs_workload::suggest` engine). A stem that
    // is NOT in the bundled catalog, so the local-file path is the match
    // (a catalog twin would dedup the file away — see the dedup test).
    let sb = Sandbox::new();
    let buried = sb.path.join("custom/nested/deep/zzlocalonly_probe.yaml");
    std::fs::create_dir_all(buried.parent().unwrap()).unwrap();
    std::fs::write(&buried, "ops: { a: { raw: x } }\n").unwrap();

    let out = Command::new(env!("CARGO_BIN_EXE_nmbrs"))
        .arg("check")
        .arg("zzlocalonly")
        .current_dir(&sb.path)
        .output()
        .expect("spawn nmbrs check");

    let stderr = String::from_utf8_lossy(&out.stderr);
    assert!(
        !out.status.success(),
        "an unresolved name must exit non-zero.\nstderr:\n{stderr}"
    );
    assert!(
        stderr.contains("Did you mean"),
        "expected a 'did you mean' suggestion, got:\nstderr:\n{stderr}"
    );
    assert!(
        stderr.contains("custom/nested/deep/zzlocalonly_probe.yaml"),
        "expected the buried leaf match in the suggestions, got:\nstderr:\n{stderr}"
    );
}

#[test]
fn nmbrs_check_dedups_repo_example_against_its_catalog_twin() {
    // A local example file under a bundle-source root duplicates its
    // embedded catalog entry. The suggestion list must offer the catalog
    // name (runnable anywhere) and NOT the redundant local path — otherwise
    // the two diverge after `examples/` and bash collapses a `<TAB>` to the
    // shared prefix, eating the operator's typed filter.
    let sb = Sandbox::new();
    // phase_poll_smoke is a real bundled example: examples/controls/phase_poll_smoke.
    let twin = sb
        .path
        .join("examples/workloads/controls/phase_poll_smoke.yaml");
    std::fs::create_dir_all(twin.parent().unwrap()).unwrap();
    std::fs::write(&twin, "ops: { a: { raw: x } }\n").unwrap();

    let out = Command::new(env!("CARGO_BIN_EXE_nmbrs"))
        .arg("check")
        .arg("phase_poll")
        .current_dir(&sb.path)
        .output()
        .expect("spawn nmbrs check");

    let stderr = String::from_utf8_lossy(&out.stderr);
    assert!(
        stderr.contains("examples/controls/phase_poll_smoke"),
        "expected the catalog name in the suggestions, got:\nstderr:\n{stderr}"
    );
    assert!(
        !stderr.contains("examples/workloads/controls/phase_poll_smoke.yaml"),
        "the redundant local twin must be deduped out, got:\nstderr:\n{stderr}"
    );
}

mod scratch;

// Copyright 2024-2026 Jonathan Shook
// SPDX-License-Identifier: Apache-2.0

//! Every auto-discovered example workload as its OWN named test.
//!
//! `harness = false`: this target speaks libtest's protocol through
//! `libtest-mimic`, so the discovery that used to live inside one 26-second
//! test (`example_workloads_in_process`) becomes one `Trial` per workload —
//! `nextest list` names them individually, `nextest run` schedules them in
//! the general mix, and a single failing workload is a single failing test
//! with its own output instead of a needle in a sweep summary.
//!
//! Each trial runs the REAL `nmbrs check workload=<file>` as a subprocess in
//! its own sandbox cwd. That is deliberately the same command the CI gate is
//! defined against ("gates exactly what `nmbrs check workload=examples`
//! checks") — same discovery of `#@ expect` rules, same run semantics, same
//! verifier. Process-per-test also dissolves the sweep's session-param
//! grouping constraint: that grouping existed only because executions SHARED
//! sessions in one process, and here every workload gets its own process and
//! its own session by construction.
//!
//! The shared-session concurrency property the old sweep also exercised is
//! not lost — it is the entire subject of `example_workloads_concurrent.rs`.

use std::path::PathBuf;

use libtest_mimic::{Arguments, Failed, Trial};

fn repo_root() -> PathBuf {
    // CARGO_MANIFEST_DIR = <repo>/nmbrs for this crate's test targets.
    PathBuf::from(env!("CARGO_MANIFEST_DIR"))
        .parent()
        .expect("nmbrs crate lives one level under the workspace root")
        .to_path_buf()
}

/// One workload, one check, one sandbox. Mirrors `check_cli.rs`: run from a
/// throwaway cwd so sessions and artifacts never land in the repo.
fn run_check(workload: PathBuf) -> Result<(), Failed> {
    let sandbox = std::env::temp_dir().join(format!(
        "nmbrs-example-case-{}-{}",
        std::process::id(),
        std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .unwrap()
            .as_nanos()
    ));
    std::fs::create_dir_all(&sandbox).map_err(|e| format!("sandbox: {e}"))?;
    let out = std::process::Command::new(env!("CARGO_BIN_EXE_nmbrs"))
        .arg("check")
        .arg(format!("workload={}", workload.display()))
        .current_dir(&sandbox)
        .output()
        .map_err(|e| format!("spawn nmbrs check: {e}"))?;
    let _ = std::fs::remove_dir_all(&sandbox);
    if out.status.success() {
        return Ok(());
    }
    Err(format!(
        "nmbrs check failed for {}:\n--- stdout ---\n{}\n--- stderr ---\n{}",
        workload.display(),
        String::from_utf8_lossy(&out.stdout),
        String::from_utf8_lossy(&out.stderr),
    )
    .into())
}

fn main() {
    let args = Arguments::from_args();
    // The whole `examples/` tree — workloads plus the module-resolution
    // demos — matching the sweep this target supersedes. The same env
    // override narrows it for fast iteration.
    let examples = std::env::var("NMBRS_TEST_EXAMPLES_DIR")
        .map(PathBuf::from)
        .unwrap_or_else(|_| repo_root().join("nmbrs").join("examples"));

    let trials: Vec<Trial> = nmbrs_workload::verify::collect_workload_files(&examples)
        .into_iter()
        .map(|file| {
            // Name by path relative to examples/, so `nextest run -E
            // 'test(optimizer/)'` selects a subtree the same way
            // NMBRS_TEST_EXAMPLES_DIR used to.
            let name = file
                .strip_prefix(&examples)
                .unwrap_or(&file)
                .display()
                .to_string();
            Trial::test(name, move || run_check(file))
        })
        .collect();

    assert!(
        !trials.is_empty(),
        "no workloads discovered under {} — the discovery breaking would \
         otherwise read as a fast green run",
        examples.display()
    );
    libtest_mimic::run(&args, trials).exit();
}

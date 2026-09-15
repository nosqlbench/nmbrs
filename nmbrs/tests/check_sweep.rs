// Copyright 2024-2026 Jonathan Shook
// SPDX-License-Identifier: Apache-2.0

//! The real `nmbrs check <directory>` sweep over the bundled
//! examples, exactly as an operator runs it.
//!
//! The in-process example harness gives every sequenced case its
//! own sandbox, so it can never see defects that need the CLI
//! walker's own execution model — concurrent worker threads and
//! the `#@ session cwd` working-directory semantics. That gap
//! hid a race where session-cwd cases sharing one cwd wiped each
//! other's `sessions/latest` between a round-trip's invocations
//! (only at full-sweep concurrency, never standalone). This test
//! closes the gap: the sweep itself is part of the workspace
//! suite.
//!
//! Sandbox discipline per `feedback_tests_no_project_root`.

use std::path::PathBuf;
use std::process::Command;

#[test]
fn cli_check_sweep_over_examples_is_green() {
    let examples: PathBuf = PathBuf::from(env!("CARGO_MANIFEST_DIR"))
        .parent()
        .expect("repo root")
        .join("nmbrs")
        .join("examples");
    assert!(
        examples.is_dir(),
        "examples dir missing: {}",
        examples.display()
    );

    let nanos = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .unwrap()
        .as_nanos();
    let sandbox =
        std::env::temp_dir().join(format!("nmbrs-checksweep-{}-{nanos}", std::process::id()));
    std::fs::create_dir_all(&sandbox).expect("create sandbox");

    let out = Command::new(env!("CARGO_BIN_EXE_nmbrs"))
        .current_dir(&sandbox)
        .arg("check")
        .arg(&examples)
        .output()
        .expect("run nmbrs check");
    let combined = format!(
        "{}{}",
        String::from_utf8_lossy(&out.stdout),
        String::from_utf8_lossy(&out.stderr),
    );
    let _ = std::fs::remove_dir_all(&sandbox);

    assert!(
        out.status.success() && !combined.contains("checks failed"),
        "nmbrs check sweep failed:\n{combined}"
    );
    assert!(
        combined.contains("checks passed"),
        "sweep summary missing:\n{combined}"
    );
}

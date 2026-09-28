// Copyright 2024-2026 Jonathan Shook
// SPDX-License-Identifier: Apache-2.0

//! `nmbrs blueprint` — authoring-support e2e.
//!
//! Contracts:
//! - `blueprint list` names the bundled blueprints with their
//!   slot counts;
//! - `blueprint template <name> <file>` writes a skeleton that
//!   BINDS against its blueprint as-is: total slot coverage, all
//!   promised `yields`/`results` wires declared, every
//!   search_perf phase traversing in dryrun without editing a
//!   single TODO;
//! - an existing output file is never overwritten.
//!
//! Sandbox discipline per `feedback_tests_no_project_root`.

use std::path::PathBuf;
use std::process::Command;

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
            std::env::temp_dir().join(format!("nmbrs-bptpl-{tag}-{}-{nanos}", std::process::id()));
        std::fs::create_dir_all(&dir).expect("create sandbox");
        Self { dir }
    }

    fn nmbrs(&self, args: &[&str]) -> (String, String, bool) {
        let out = Command::new(env!("CARGO_BIN_EXE_nmbrs"))
            .current_dir(&self.dir)
            .args(args)
            .output()
            .expect("run nmbrs");
        (
            String::from_utf8_lossy(&out.stdout).to_string(),
            String::from_utf8_lossy(&out.stderr).to_string(),
            out.status.success(),
        )
    }
}

impl Drop for Sandbox {
    fn drop(&mut self) {
        let _ = std::fs::remove_dir_all(&self.dir);
    }
}

#[test]
fn list_names_bundled_blueprints_with_slot_counts() {
    let sandbox = Sandbox::new("list");
    let (stdout, stderr, ok) = sandbox.nmbrs(&["blueprint", "list"]);
    assert!(ok, "list must succeed; stderr:\n{stderr}");
    let line = stdout
        .lines()
        .find(|l| l.starts_with("vector_suite_blueprint"))
        .unwrap_or_else(|| panic!("vector_suite_blueprint missing from listing:\n{stdout}"));
    assert!(line.contains("24"), "twenty-four slots; line: {line}");
}

/// The headline contract: a freshly generated skeleton — no TODO
/// filled — binds into its blueprint and traverses every phase.
#[test]
fn generated_template_binds_and_traverses_unedited() {
    let sandbox = Sandbox::new("gen");
    let (stdout, stderr, ok) = sandbox.nmbrs(&[
        "blueprint",
        "template",
        "vector_suite_blueprint",
        "impl.yaml",
    ]);
    assert!(
        ok,
        "template must succeed; stdout:\n{stdout}\nstderr:\n{stderr}"
    );
    assert!(stdout.contains("24 slot(s)"), "stdout:\n{stdout}");

    let session = sandbox.dir.join("session");
    let (stdout, stderr, ok) = sandbox.nmbrs(&[
        "run",
        "workload=impl.yaml",
        "scenario=search_perf",
        "adapter=stdout",
        "tui=off",
        "dryrun=phases",
        "--session-path",
        session.to_str().expect("utf8 path"),
    ]);
    assert!(
        ok,
        "dryrun must complete; stdout:\n{stdout}\nstderr:\n{stderr}"
    );
    let all = format!("{stdout}\n{stderr}");
    assert!(
        all.contains("into blueprint 'vector_suite_blueprint'"),
        "the skeleton binds; output:\n{all}"
    );
    assert!(
        all.contains("12 completed, 0 failed"),
        "all phases traverse unedited; output:\n{all}"
    );
}

#[test]
fn template_refuses_to_overwrite() {
    let sandbox = Sandbox::new("noclobber");
    std::fs::write(sandbox.dir.join("impl.yaml"), "# precious\n").expect("write existing file");
    let (_, stderr, ok) = sandbox.nmbrs(&[
        "blueprint",
        "template",
        "vector_suite_blueprint",
        "impl.yaml",
    ]);
    assert!(!ok, "overwrite must be refused");
    assert!(
        stderr.contains("refusing to overwrite"),
        "stderr:\n{stderr}"
    );
    let kept = std::fs::read_to_string(sandbox.dir.join("impl.yaml")).expect("read kept file");
    assert_eq!(kept, "# precious\n", "existing content untouched");
}

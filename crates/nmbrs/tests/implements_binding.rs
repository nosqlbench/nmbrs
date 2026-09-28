// Copyright 2024-2026 Jonathan Shook
// SPDX-License-Identifier: Apache-2.0

//! SRD-108 Part B e2e — typed `abstract:`/`implements:` binding.
//!
//! Contracts:
//! - `workload=<impl>` pulls the blueprint via `implements:`
//!   and runs the bound composition (scaffolding from the blueprint, op bodies from the implementation);
//! - `workload=<blueprint> impl=<impl>` reaches the same bound
//!   composition;
//! - a blueprint invoked with NO implementation fails at
//!   load, naming the unbound slot and the `impl=` remedy;
//! - an `impl=` target that implements a DIFFERENT blueprint is
//!   rejected with both identities named.
//!
//! Sandbox discipline per `feedback_tests_no_project_root`.

use std::path::PathBuf;
use std::process::Command;

const BLUEPRINT: &str = r#"
params:
  suite_k: "7"
phases:
  probe:
    cycles: 3
    concurrency: 1
    ops:
      search:
        abstract:
          needs:
            suite_k: u64
"#;

const IMPL: &str = r#"
implements: ./blueprint.yaml
phases:
  probe:
    ops:
      search:
        stmt: "SEARCH k={suite_k}"
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
        let dir = std::env::temp_dir().join(format!(
            "nmbrs-implements-{tag}-{}-{nanos}",
            std::process::id()
        ));
        std::fs::create_dir_all(&dir).expect("create sandbox");
        std::fs::write(dir.join("blueprint.yaml"), BLUEPRINT).expect("write blueprint");
        std::fs::write(dir.join("impl.yaml"), IMPL).expect("write impl");
        Self { dir }
    }

    fn run(&self, args: &[&str]) -> (String, String, bool) {
        let mut cmd = Command::new(env!("CARGO_BIN_EXE_nmbrs"));
        cmd.current_dir(&self.dir)
            .arg("run")
            .arg("tui=off")
            .arg("--session-path")
            .arg(self.dir.join("session"));
        for a in args {
            cmd.arg(a);
        }
        let out = cmd.output().expect("run nmbrs");
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

fn ticks(stdout: &str, needle: &str) -> usize {
    stdout.lines().filter(|l| l.trim() == needle).count()
}

/// `workload=<impl>` — the implementation pulls its blueprint;
/// the run carries the blueprint scaffolding (3 cycles)
/// with the implementation's op body ({suite_k} resolves from
/// the blueprint param).
#[test]
fn invoking_the_implementation_pulls_and_binds_the_blueprint() {
    let sandbox = Sandbox::new("pull");
    let (stdout, stderr, ok) = sandbox.run(&["workload=impl.yaml"]);
    assert!(ok, "bound run must complete; stderr:\n{stderr}");
    assert_eq!(
        ticks(&stdout, "SEARCH k=7"),
        3,
        "blueprint cycles × implementation body; stdout:\n{stdout}"
    );
}

/// `workload=<blueprint> impl=<impl>` — same bound composition
/// from the blueprint entry point.
#[test]
fn invoking_the_blueprint_with_impl_binds() {
    let sandbox = Sandbox::new("implparam");
    let (stdout, stderr, ok) = sandbox.run(&["workload=blueprint.yaml", "impl=impl.yaml"]);
    assert!(ok, "bound run must complete; stderr:\n{stderr}");
    assert_eq!(ticks(&stdout, "SEARCH k=7"), 3, "stdout:\n{stdout}");
}

/// A blueprint with no implementation fails at LOAD,
/// naming the slot and the remedy — never a dispatch panic.
#[test]
fn unbound_abstract_slot_is_a_load_error() {
    let sandbox = Sandbox::new("unbound");
    let (_, stderr, ok) = sandbox.run(&["workload=blueprint.yaml"]);
    assert!(!ok, "an unbound blueprint must not run");
    assert!(
        stderr.contains("probe.search") && stderr.contains("impl="),
        "the error names the slot and the remedy; stderr:\n{stderr}"
    );
}

/// An `impl=` whose `implements:` names a DIFFERENT blueprint is
/// rejected with both identities in the message.
#[test]
fn mismatched_implements_target_is_rejected() {
    let sandbox = Sandbox::new("mismatch");
    std::fs::write(sandbox.dir.join("other.yaml"), BLUEPRINT).expect("write other blueprint");
    let (_, stderr, ok) = sandbox.run(&["workload=other.yaml", "impl=impl.yaml"]);
    assert!(!ok, "a mismatched implements target must not run");
    assert!(
        stderr.contains("implements"),
        "the error explains the target mismatch; stderr:\n{stderr}"
    );
}

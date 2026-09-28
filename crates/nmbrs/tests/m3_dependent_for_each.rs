// Copyright 2024-2026 Jonathan Shook
// SPDX-License-Identifier: Apache-2.0

//! Integration tests for SRD-18b §"Iteration variables as scope
//! outputs" — M3.3 dependent-tuple iteration through per-scope
//! Polydat Kernels.
//!
//! These tests exercise the full pipeline (YAML → ScopeTree
//! synthesis → kernel install → dispatcher → stdout adapter)
//! to validate that:
//!
//! - Multi-clause `for_each` with dependent name composition
//!   (`{k_{k}_limits}`) enumerates the expected tuples.
//! - Iteration variables flow as scope outputs through the
//!   kernel's standard `bind_outer_scope` chain — children see
//!   them via auto-extern, with native types preserved.
//! - `for_each_union` across sub-spaces enumerates the
//!   concatenation of per-sub-space tuple-spaces.
//!
//! Tests run the `nmbrs` binary against synthesized YAML
//! workloads and assert on stdout content. Not unit tests —
//! they ARE the contract for the user-visible feature.

use std::io::Write;
use std::path::PathBuf;
use std::process::Command;

fn nmbrs() -> Command {
    let mut cmd = Command::new(env!("CARGO_BIN_EXE_nmbrs"));
    let workspace_root = std::path::Path::new(env!("CARGO_MANIFEST_DIR"))
        .parent()
        .unwrap()
        .parent()
        .expect("workspace root");
    cmd.current_dir(workspace_root);
    cmd.env("HOME", "/nonexistent");
    cmd
}

/// Build a unique temp session-path so cargo's parallel test
/// execution doesn't collide on `logs/default_<timestamp>`.
/// Caller passes the returned path to `--session-path`; the
/// `_guard` cleans up on drop.
struct SessionGuard {
    path: PathBuf,
    parent: PathBuf,
}

impl SessionGuard {
    fn new(label: &str) -> Self {
        let parent = std::env::temp_dir().join(format!(
            "nmbrs-m3-{label}-{}-{}",
            std::process::id(),
            std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .unwrap()
                .as_nanos(),
        ));
        std::fs::create_dir_all(&parent).expect("create session parent");
        let path = parent.join("session");
        Self { path, parent }
    }
}

impl Drop for SessionGuard {
    fn drop(&mut self) {
        let _ = std::fs::remove_dir_all(&self.parent);
    }
}

fn write_workload(label: &str, body: &str) -> PathBuf {
    let mut dir = std::env::temp_dir();
    std::fs::create_dir_all(&dir).expect("temp dir create");
    dir.push(format!(
        "nmbrs_m3_{label}_{}_{}.yaml",
        std::process::id(),
        std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .unwrap()
            .as_nanos(),
    ));
    let mut f = std::fs::File::create(&dir).expect("workload file create");
    f.write_all(body.as_bytes()).expect("workload file write");
    dir
}

/// Multi-clause `for_each` with dependent name composition:
/// clause 2's spec text references clause 1's iter var to
/// dynamically construct an outer parameter name. The
/// dispatcher should evaluate clause 1 to get k's values, then
/// for each k evaluate clause 2 with `{k}` resolved to the
/// current k. Emits one stdout line per (k, limit) tuple; total
/// lines = Σ |k_v_limits| over v ∈ k_values.
#[test]
fn dependent_tuple_with_dynamic_name_composition() {
    // Workload-level bindings declare each value list as an
    // output of the workload kernel — visible in its manifest
    // and therefore inheritable by the for_each scope's
    // synthesized kernel via standard bind_outer_scope. This is
    // what M3.3 exercises: the dependent-tuple dispatcher
    // resolving placeholders through Polydat's name space.
    // M3.3 routes for_each spec interpolation through the
    // installed scope kernel; workload params get injected as
    // `const` bindings on the synthesized kernel directly
    // (the M3.3-bridge until M3.6 promotes them to workload-
    // kernel `const` bindings).
    let yaml = r#"
params:
  k_values: "1, 10"
  k_1_limits: "100, 200, 300, 400"
  k_10_limits: "1000, 2000, 3000"

scenarios:
  default:
    - for_each: "k in {k_values}, limit in {k_{k}_limits}"
      phases: [emit]

phases:
  emit:
    adapter: stdout
    cycles: 1
    concurrency: 1
    bindings: |
      c := (cycle)
      _used_k1 := "{k_1_limits}"
      _used_k10 := "{k_10_limits}"
    ops:
      out:
        stmt: "k={k} limit={limit}"
"#;
    let path = write_workload("dependent", yaml);
    let session = SessionGuard::new("dependent");
    let output = nmbrs()
        .args([
            "run",
            &format!("workload={}", path.display()),
            "scenario=default",
        ])
        .arg("--session-path")
        .arg(&session.path)
        .output()
        .expect("nmbrs failed to start");
    let stdout = String::from_utf8_lossy(&output.stdout).to_string();
    let stderr = String::from_utf8_lossy(&output.stderr).to_string();
    assert!(
        output.status.success(),
        "nmbrs failed:\nstdout: {stdout}\nstderr: {stderr}"
    );

    // Total: |k=1's limits| + |k=10's limits| = 4 + 3 = 7
    let emit_lines: Vec<&str> = stdout.lines().filter(|l| l.starts_with("k=")).collect();
    assert_eq!(
        emit_lines.len(),
        7,
        "expected 7 emit lines, got {}:\n{stdout}",
        emit_lines.len()
    );

    // Every k=1 line should pair with a limit from {100, 200, 300, 400}.
    let k1: Vec<&&str> = emit_lines
        .iter()
        .filter(|l| l.starts_with("k=1 "))
        .collect();
    let k10: Vec<&&str> = emit_lines
        .iter()
        .filter(|l| l.starts_with("k=10 "))
        .collect();
    assert_eq!(k1.len(), 4, "k=1 should pair with 4 limits, got: {k1:?}");
    assert_eq!(k10.len(), 3, "k=10 should pair with 3 limits, got: {k10:?}");

    // No k=1 line should reference k=10's limits or vice versa.
    for line in &k1 {
        assert!(
            !line.contains("limit=1000")
                && !line.contains("limit=2000")
                && !line.contains("limit=3000"),
            "k=1 line referenced a k=10 limit value: {line}"
        );
    }
    for line in &k10 {
        assert!(
            !line.contains("limit=100 ")
                && !line.contains("limit=200 ")
                && !line.contains("limit=300 ")
                && !line.contains("limit=400"),
            "k=10 line referenced a k=1 limit value: {line}"
        );
    }

    // Cleanup
    let _ = std::fs::remove_file(&path);
}

/// Regression (SRD-18f Stage 2): a dependent comprehension whose
/// *prior* clause is a **bare** reference (`k in k_values`, not
/// `{k_values}`), and whose dependent clause composes the prior
/// iter-var into a dynamic param name (`limit in {k_{k}_limits}`).
///
/// This is the `query_sweep` shape. The synthesis-time type
/// inference (`pre_evaluate_clause`) must resolve the bare prior
/// clause so the dependent clause's `{k}` substitution lands the
/// *typed* value — otherwise `limit` defaulted to `String` and a
/// downstream u64 use (`mul(limit, 2)`) failed phase-kernel
/// synthesis with "cannot connect String output to u64 input".
/// The existing dependent test uses the braced prior form
/// (`{k_values}`), which masked this gap.
#[test]
fn dependent_bare_prior_clause_types_dependent_iter_var_numerically() {
    let yaml = r#"
params:
  k_values: "1, 10"
  k_1_limits: "100, 200"
  k_10_limits: "1000, 2000, 3000"

scenarios:
  default:
    - for_each: "k in k_values, limit in {k_{k}_limits}"
      phases: [emit]

phases:
  emit:
    adapter: stdout
    cycles: 1
    concurrency: 1
    bindings: |
      # Downstream u64 use: if `limit` mis-typed as String, this
      # arithmetic fails phase-kernel synthesis.
      doubled := mul(limit, 2)
      _used_k1 := "{k_1_limits}"
      _used_k10 := "{k_10_limits}"
    ops:
      out:
        stmt: "k={k} limit={limit} doubled={doubled}"
"#;
    let path = write_workload("dependent_bare", yaml);
    let session = SessionGuard::new("dependent_bare");
    let output = nmbrs()
        .args([
            "run",
            &format!("workload={}", path.display()),
            "scenario=default",
        ])
        .arg("--session-path")
        .arg(&session.path)
        .output()
        .expect("nmbrs failed to start");
    let stdout = String::from_utf8_lossy(&output.stdout).to_string();
    let stderr = String::from_utf8_lossy(&output.stderr).to_string();
    assert!(
        output.status.success(),
        "bare-prior-clause dependent comprehension failed to synthesize \
         (limit likely mis-typed as String):\nstdout: {stdout}\nstderr: {stderr}"
    );

    // |k=1's limits| + |k=10's limits| = 2 + 3 = 5
    let emit_lines: Vec<&str> = stdout.lines().filter(|l| l.starts_with("k=")).collect();
    assert_eq!(emit_lines.len(), 5, "expected 5 tuples, got:\n{stdout}");
    // The downstream u64 doubling proves numeric typing held.
    assert!(
        stdout.contains("limit=100 doubled=200"),
        "k=1 first: {stdout}"
    );
    assert!(
        stdout.contains("limit=1000 doubled=2000"),
        "k=10 first: {stdout}"
    );

    let _ = std::fs::remove_file(&path);
}

/// Multi-clause non-dependent (Cartesian product). No clause
/// references another, so the dispatcher should produce |a| ×
/// |b| tuples.
#[test]
fn multi_clause_independent_cartesian_product() {
    let yaml = r#"
params:
  alphas: "a, b, c"
  nums: "1, 2"

scenarios:
  default:
    - for_each: "alpha in {alphas}, num in {nums}"
      phases: [emit]

phases:
  emit:
    adapter: stdout
    cycles: 1
    concurrency: 1
    bindings: |
      c := (cycle)
    ops:
      out:
        stmt: "tuple={alpha}/{num}"
"#;
    let path = write_workload("cartesian", yaml);
    let session = SessionGuard::new("cartesian");
    let output = nmbrs()
        .args([
            "run",
            &format!("workload={}", path.display()),
            "scenario=default",
        ])
        .arg("--session-path")
        .arg(&session.path)
        .output()
        .expect("nmbrs failed to start");
    let stdout = String::from_utf8_lossy(&output.stdout).to_string();
    let stderr = String::from_utf8_lossy(&output.stderr).to_string();
    assert!(
        output.status.success(),
        "nmbrs failed:\nstdout: {stdout}\nstderr: {stderr}"
    );

    let tuple_lines: Vec<&str> = stdout.lines().filter(|l| l.starts_with("tuple=")).collect();
    // 3 alphas × 2 nums = 6 tuples
    assert_eq!(
        tuple_lines.len(),
        6,
        "expected 6 cartesian tuples, got {}:\n{stdout}",
        tuple_lines.len()
    );

    // Spot-check a few combinations are present.
    let body = tuple_lines.join("\n");
    for expected in ["tuple=a/1", "tuple=a/2", "tuple=b/1", "tuple=c/2"] {
        assert!(
            body.contains(expected),
            "missing combination {expected}:\n{body}"
        );
    }

    let _ = std::fs::remove_file(&path);
}

/// `for_each_union`: same variable names appear across multiple
/// sub-spaces, signaling "iterate the concatenation of these
/// tuple-spaces" rather than the Cartesian product.
#[test]
fn for_each_union_across_subspaces() {
    let yaml = r#"
scenarios:
  default:
    - for_each:
        - "x in 1, 2, y in a, b"
        - "x in 9, y in 'z'"
      phases: [emit]

phases:
  emit:
    adapter: stdout
    cycles: 1
    concurrency: 1
    bindings: |
      c := (cycle)
    ops:
      out:
        stmt: "tup=x{x}y{y}"
"#;
    let path = write_workload("union", yaml);
    let session = SessionGuard::new("union");
    let output = nmbrs()
        .args([
            "run",
            &format!("workload={}", path.display()),
            "scenario=default",
        ])
        .arg("--session-path")
        .arg(&session.path)
        .output()
        .expect("nmbrs failed to start");
    let stdout = String::from_utf8_lossy(&output.stdout).to_string();
    let stderr = String::from_utf8_lossy(&output.stderr).to_string();
    assert!(
        output.status.success(),
        "nmbrs failed:\nstdout: {stdout}\nstderr: {stderr}"
    );

    let lines: Vec<&str> = stdout.lines().filter(|l| l.starts_with("tup=")).collect();
    // Sub-space 1 = {1,2} × {a,b} = 4 tuples
    // Sub-space 2 = {9} × {z}    = 1 tuple
    // Union total = 5
    assert_eq!(
        lines.len(),
        5,
        "expected 5 union tuples (4+1), got {}:\n{stdout}",
        lines.len()
    );

    let body = lines.join("\n");
    for expected in ["tup=x1ya", "tup=x1yb", "tup=x2ya", "tup=x2yb", "tup=x9yz"] {
        assert!(
            body.contains(expected),
            "missing sub-space tuple {expected}:\n{body}"
        );
    }

    let _ = std::fs::remove_file(&path);
}

/// SRD-13f Push E: a phase declaring BOTH `for_each:` and
/// `bindings:` folds the bindings into the for_each scope
/// kernel. The phase-level `bindings:` references the iter var,
/// and resolves per-iteration to the correct value. Pre-Push E,
/// the install loop's for_each branch silently dropped the
/// phase's `bindings:` block (only the parser-merge fallback
/// kept those workloads working incidentally — and Push D
/// retired that fallback).
#[test]
fn phase_with_for_each_and_bindings_folds_bindings_into_loop_scope() {
    let yaml = r#"
scenarios:
  default: [run]

phases:
  run:
    adapter: stdout
    cycles: 1
    concurrency: 1
    for_each: "k in 1, 2, 3"
    bindings: |
      doubled := mul(k, 2)
    ops:
      out:
        stmt: "k={k} doubled={doubled}"
"#;
    let path = write_workload("phase_for_each_bindings", yaml);
    let session = SessionGuard::new("phase_for_each_bindings");
    let output = nmbrs()
        .args([
            "run",
            &format!("workload={}", path.display()),
            "scenario=default",
        ])
        .arg("--session-path")
        .arg(&session.path)
        .output()
        .expect("nmbrs failed to start");
    let stdout = String::from_utf8_lossy(&output.stdout).to_string();
    let stderr = String::from_utf8_lossy(&output.stderr).to_string();
    assert!(
        output.status.success(),
        "nmbrs failed:\nstdout: {stdout}\nstderr: {stderr}"
    );

    let lines: Vec<&str> = stdout.lines().filter(|l| l.starts_with("k=")).collect();
    assert_eq!(
        lines.len(),
        3,
        "expected one line per iter-var value (3 lines), got {}:\n{stdout}",
        lines.len()
    );
    for expected in ["k=1 doubled=2", "k=2 doubled=4", "k=3 doubled=6"] {
        assert!(
            lines.iter().any(|l| l.trim() == expected),
            "missing line '{expected}':\n{stdout}"
        );
    }

    let _ = std::fs::remove_file(&path);
}

// Copyright 2024-2026 Jonathan Shook
// SPDX-License-Identifier: Apache-2.0

//! Integration tests for inline workload (`op=`) support.
//!
//! These tests invoke the `nmbrs` binary with `op=` parameters and
//! verify the output. They exercise the full pipeline: inline
//! synthesis → Polydat compilation → op assembly → adapter execution.

use std::process::Command;

fn nmbrs() -> Command {
    let mut cmd = Command::new(env!("CARGO_BIN_EXE_nmbrs"));
    // Prevent auto-discovery of a running web instance.
    cmd.env("HOME", "/nonexistent");
    cmd
}

fn run_inline(op: &str, cycles: u64) -> (String, String, bool) {
    // Per-invocation `--session-path` so cargo's parallel test
    // execution doesn't race on the default `logs/default_<ts>`.
    let session_parent = std::env::temp_dir().join(format!(
        "nmbrs-inline-{}-{}-{}",
        scratch::seq(),
        std::process::id(),
        std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .unwrap()
            .as_nanos(),
    ));
    std::fs::create_dir_all(&session_parent).expect("create session parent");
    let session_path = session_parent.join("session");
    let output = nmbrs()
        .args(["run", &format!("op={op}"), &format!("cycles={cycles}")])
        .arg("--session-path")
        .arg(&session_path)
        .output()
        .expect("failed to execute nmbrs");
    let _ = std::fs::remove_dir_all(&session_parent);
    let stdout = String::from_utf8_lossy(&output.stdout).to_string();
    let stderr = String::from_utf8_lossy(&output.stderr).to_string();
    (stdout, stderr, output.status.success())
}

#[test]
fn simple_cycle_output() {
    let (stdout, stderr, ok) = run_inline("hello {{cycle}}", 5);
    assert!(ok, "nmbrs failed: {stderr}");
    let lines: Vec<&str> = stdout.lines().collect();
    assert_eq!(lines.len(), 5, "expected 5 lines, got: {stdout}");
    assert_eq!(lines[0], "hello 0");
    assert_eq!(lines[4], "hello 4");
}

#[test]
fn hash_and_mod_binding() {
    let (stdout, stderr, ok) = run_inline("id={{mod(hash(cycle), 1000)}}", 3);
    assert!(ok, "nmbrs failed: {stderr}");
    let lines: Vec<&str> = stdout.lines().collect();
    assert_eq!(lines.len(), 3);
    for line in &lines {
        assert!(line.starts_with("id="), "unexpected line: {line}");
        let id_str = line.strip_prefix("id=").unwrap();
        let id: u64 = id_str.parse().expect("id should be a number");
        assert!(id < 1000, "id should be < 1000, got {id}");
    }
}

#[test]
fn multiple_inline_bindings() {
    let (stdout, stderr, ok) = run_inline("a={{mod(hash(cycle), 100)}} b={{add(cycle, 1000)}}", 3);
    assert!(ok, "nmbrs failed: {stderr}");
    let lines: Vec<&str> = stdout.lines().collect();
    assert_eq!(lines.len(), 3);
    for line in &lines {
        assert!(line.contains("a="), "missing a= in: {line}");
        assert!(line.contains("b="), "missing b= in: {line}");
    }
    // b should be 1000, 1001, 1002 for cycles 0, 1, 2.
    assert!(
        lines[0].contains("b=1000"),
        "expected b=1000, got: {}",
        lines[0]
    );
    assert!(
        lines[2].contains("b=1002"),
        "expected b=1002, got: {}",
        lines[2]
    );
}

#[test]
fn semicolon_multiple_ops() {
    let (stdout, stderr, ok) = run_inline("read {{cycle}};write {{cycle}}", 4);
    assert!(ok, "nmbrs failed: {stderr}");
    let lines: Vec<&str> = stdout.lines().collect();
    assert_eq!(lines.len(), 4);
    // With bucket sequencer and 1:1 ratio, stanza is [read, write].
    // Cycle 0 → read, cycle 1 → write, cycle 2 → read, cycle 3 → write.
    assert!(
        lines[0].starts_with("read "),
        "expected read, got: {}",
        lines[0]
    );
    assert!(
        lines[1].starts_with("write "),
        "expected write, got: {}",
        lines[1]
    );
    assert!(
        lines[2].starts_with("read "),
        "expected read, got: {}",
        lines[2]
    );
    assert!(
        lines[3].starts_with("write "),
        "expected write, got: {}",
        lines[3]
    );
}

#[test]
fn ratio_prefix() {
    let (stdout, stderr, ok) = run_inline("3:A {{cycle}};1:B {{cycle}}", 8);
    assert!(ok, "nmbrs failed: {stderr}");
    let lines: Vec<&str> = stdout.lines().collect();
    assert_eq!(lines.len(), 8);
    // Stanza length = 4 (3 + 1). Pattern repeats: A, B, A, A, A, B, A, A.
    let a_count = lines.iter().filter(|l| l.starts_with("A ")).count();
    let b_count = lines.iter().filter(|l| l.starts_with("B ")).count();
    assert_eq!(a_count, 6, "expected 6 A ops, got {a_count}");
    assert_eq!(b_count, 2, "expected 2 B ops, got {b_count}");
}

#[test]
fn plain_text_no_bindings() {
    let (stdout, stderr, ok) = run_inline("static text", 3);
    assert!(ok, "nmbrs failed: {stderr}");
    let lines: Vec<&str> = stdout.lines().collect();
    assert_eq!(lines.len(), 3);
    assert_eq!(lines[0], "static text");
    assert_eq!(lines[2], "static text");
}

#[test]
fn reference_bind_point_cycle() {
    // {cycle} without double braces — resolved as coordinate reference.
    let (stdout, stderr, ok) = run_inline("c={cycle}", 3);
    assert!(ok, "nmbrs failed: {stderr}");
    let lines: Vec<&str> = stdout.lines().collect();
    assert_eq!(lines[0], "c=0");
    assert_eq!(lines[2], "c=2");
}

#[test]
fn json_format() {
    // Per-invocation `--session-path` so nextest's parallel test
    // execution doesn't race on the default `logs/default_<ts>`
    // session directory (see `feedback_tests_no_project_root`).
    let session_parent = std::env::temp_dir().join(format!(
        "nmbrs-json-format-{}-{}-{}",
        scratch::seq(),
        std::process::id(),
        std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .unwrap()
            .as_nanos(),
    ));
    std::fs::create_dir_all(&session_parent).expect("create session parent");
    let session_path = session_parent.join("session");
    let output = nmbrs()
        .args(["run", "op=val={{cycle}}", "cycles=2", "format=json"])
        .arg("--session-path")
        .arg(&session_path)
        .output()
        .expect("failed to execute nmbrs");
    let _ = std::fs::remove_dir_all(&session_parent);
    assert!(
        output.status.success(),
        "nmbrs failed: {}",
        String::from_utf8_lossy(&output.stderr)
    );
    let stdout = String::from_utf8_lossy(&output.stdout);
    let lines: Vec<&str> = stdout.lines().collect();
    assert_eq!(lines.len(), 2);
    // Should be valid JSON.
    for line in &lines {
        let _: serde_json::Value =
            serde_json::from_str(line).unwrap_or_else(|e| panic!("invalid JSON '{line}': {e}"));
    }
}

#[test]
fn dry_run_fields() {
    let session_parent = std::env::temp_dir().join(format!(
        "nmbrs-inline-dry-{}-{}-{}",
        scratch::seq(),
        std::process::id(),
        std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .unwrap()
            .as_nanos(),
    ));
    std::fs::create_dir_all(&session_parent).expect("create session parent");
    let session_path = session_parent.join("session");
    let output = nmbrs()
        .args(["run", "op=test {{cycle}}", "cycles=3", "dryrun=fields"])
        .arg("--session-path")
        .arg(&session_path)
        .output()
        .expect("failed to execute nmbrs");
    let _ = std::fs::remove_dir_all(&session_parent);
    assert!(output.status.success(), "nmbrs failed");
    let stdout = String::from_utf8_lossy(&output.stdout);
    let lines: Vec<&str> = stdout.lines().collect();
    assert_eq!(lines.len(), 3);
    assert_eq!(lines[0], "test 0");
}

#[test]
fn deterministic_output() {
    // Same op + cycles should always produce the same output.
    let (out1, _, ok1) = run_inline("v={{mod(hash(cycle), 10000)}}", 5);
    let (out2, _, ok2) = run_inline("v={{mod(hash(cycle), 10000)}}", 5);
    assert!(ok1 && ok2);
    assert_eq!(out1, out2, "output should be deterministic");
}

#[test]
fn empty_op_fails() {
    let session_parent = std::env::temp_dir().join(format!(
        "nmbrs-inline-empty-{}-{}-{}",
        scratch::seq(),
        std::process::id(),
        std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .unwrap()
            .as_nanos(),
    ));
    std::fs::create_dir_all(&session_parent).expect("create session parent");
    let session_path = session_parent.join("session");
    let output = nmbrs()
        .args(["run", "op=", "cycles=1"])
        .arg("--session-path")
        .arg(&session_path)
        .output()
        .expect("failed to execute nmbrs");
    let _ = std::fs::remove_dir_all(&session_parent);
    assert!(!output.status.success(), "empty op should fail");
}

#[test]
fn op_overrides_workload_with_warning() {
    let session_parent = std::env::temp_dir().join(format!(
        "nmbrs-inline-override-{}-{}-{}",
        scratch::seq(),
        std::process::id(),
        std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .unwrap()
            .as_nanos(),
    ));
    std::fs::create_dir_all(&session_parent).expect("create session parent");
    let session_path = session_parent.join("session");
    let output = nmbrs()
        .args([
            "run",
            "op=hello {{cycle}}",
            "workload=nonexistent.yaml",
            "cycles=1",
        ])
        .arg("--session-path")
        .arg(&session_path)
        .output()
        .expect("failed to execute nmbrs");
    let _ = std::fs::remove_dir_all(&session_parent);
    assert!(output.status.success(), "op= should override workload=");
    let stderr = String::from_utf8_lossy(&output.stderr);
    assert!(stderr.contains("warning"), "should warn about override");
    let stdout = String::from_utf8_lossy(&output.stdout);
    assert_eq!(stdout.trim(), "hello 0");
}

mod scratch;
